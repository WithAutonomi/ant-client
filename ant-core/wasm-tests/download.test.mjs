import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient, encryptPublicFile } from "./pkg/ant_core.js";
import { mockWebRtc, paymentNetwork } from "./mock-webrtc.mjs";

const content = new TextEncoder().encode("Discoverable storage holder regression.".repeat(100));
const encrypted = encryptPublicFile(content);
const datamap = encrypted.records.at(-1);

function failDiscovery(channel, method) {
  if (method !== "find_node") return;
  // Fail only discovery, not authenticated GET. A new association can still
  // serve the file, as with a transient FIND_NODE timeout in a real network.
  channel.emit(new ArrayBuffer(0));
  return false;
}

test("a known holder remains readable when its latest discovery request fails", async () => {
  const rtc = mockWebRtc([{ chunk: datamap.content, respond: failDiscovery }, {}]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(datamap.address);
    assert.equal(reader.size, content.length);
    assert(rtc.requests.some(request => request.node === 0 && request.method === "get_chunk"));
  } finally { reader?.close(); client.close(); }
});

test("GET can recover even when no node answers discovery", async () => {
  const rtc = mockWebRtc([{ chunk: datamap.content, respond: failDiscovery }]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(datamap.address);
    assert.equal(reader.size, content.length);
  } finally { reader?.close(); client.close(); }
});


test("fallback is bounded and never accepts a chunk with the wrong hash", async () => {
  const rtc = mockWebRtc(Array.from({ length: 24 }, () => ({ chunk: new Uint8Array([1, 2, 3]), respond: failDiscovery })));
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    await assert.rejects(client.openPublicFile(datamap.address), /BLAKE3 mismatch/);
    const gets = rtc.requests.filter(request => request.method === "get_chunk");
    assert.equal(gets.length, 40, "try at most twenty additional endpoints per native retry sweep");
    assert.equal(new Set(gets.map(request => request.node)).size, 20, "deduplicate peers within each sweep");
    for (const node of new Set(gets.map(request => request.node))) {
      assert.equal(gets.filter(request => request.node === node).length, 2);
    }
  } finally { client.close(); }
});


test("a just-uploaded file downloads and streams after its holders fail discovery", async () => {
  const nodes = Array.from({ length: 7 }, () => ({}));
  const rtc = mockWebRtc(nodes);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    const uploaded = await client.uploadPublicFile(content, "roundtrip.txt", "text/plain", paymentNetwork, async (_, quotes) => ({
      transactionHash: `0x${"ab".repeat(32)}`,
      totalAmount: quotes.reduce((sum, quote) => sum + BigInt(quote.amount), 0n).toString(),
    }));
    assert.equal(uploaded.file.replicas, 4);
    const holders = rtc.stores.map((records, index) => records.has(uploaded.file.address) ? index : -1).filter(index => index >= 0);
    assert.equal(holders.length, 4);
    for (const index of holders) nodes[index].respond = failDiscovery;
    const downloaded = await client.downloadPublicFile(uploaded.file, 3);
    assert.deepEqual(downloaded.content, content);
    reader = await client.openPublicFile(uploaded.file.address);
    assert.deepEqual(await reader.readRange(0, content.length), content);
  } finally { reader?.close(); client.close(); }
});
