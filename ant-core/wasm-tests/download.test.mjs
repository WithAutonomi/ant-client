import { BrowserNetworkClient } from "./client-fixture.mjs";
import assert from "node:assert/strict";
import test from "node:test";
import { encryptPublicFile, parseWebRtcDirectMultiaddr } from "./pkg/ant_core.js";
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

test("a connected known holder can finish a verified read while discovery is still pending", async () => {
  const options = [{ chunk: datamap.content }];
  const rtc = mockWebRtc(options);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader, deadline;
  try {
    await client.findClosest(datamap.address);
    // GET remains responsive while FIND_NODE cannot finish. The same shared
    // read policy is used by native, and a miss would still await discovery.
    options[0].respond = (_channel, method) => method === "find_node" ? false : undefined;
    reader = await Promise.race([
      client.openPublicFile(datamap.address),
      new Promise((_, reject) => deadline = setTimeout(() => reject(new Error("read waited for discovery")), 1000)),
    ]);
    assert.equal(reader.size, content.length);
    assert.equal(rtc.connections.length, 1);
    assert.equal(rtc.requests.filter(request => request.method === "get_chunk").length, 1);
  } finally { clearTimeout(deadline); reader?.close(); client.close(); }
});


test("a newly discovered holder is readable before a slow lookup round finishes", async () => {
  const nodes = [{ view: [1, 2] }, {}, {}];
  const rtc = mockWebRtc(nodes);
  const distance = index => BigInt(`0x${parseWebRtcDirectMultiaddr(rtc.endpoints[index]).peerId}`) ^ BigInt(`0x${datamap.address}`);
  const holder = distance(1) < distance(2) ? 1 : 2;
  nodes[holder].chunk = datamap.content;
  nodes[3 - holder].delay = method => method === "find_node" ? 2500 : 0;
  const client = new BrowserNetworkClient(rtc.endpoints.slice(0, 1));
  let reader, deadline;
  try {
    reader = await Promise.race([
      client.openPublicFile(datamap.address),
      new Promise((_, reject) => deadline = setTimeout(() => reject(new Error("read waited for lookup round")), 1500)),
    ]);
    assert.equal(reader.size, content.length);
    assert.ok(rtc.requests.some(r => r.node === holder && r.method === "get_chunk"));
  } finally { clearTimeout(deadline); reader?.close(); client.close(); }
});

test("early misses do not exhaust the fallback allowance before a later holder", async () => {
  const nodes = Array.from({ length: 12 }, () => ({
    respond: (_channel, method) => method === "find_node" ? false : undefined,
  }));
  const rtc = mockWebRtc(nodes);
  const target = BigInt(`0x${datamap.address}`);
  const ordered = rtc.endpoints.map((endpoint, index) => ({index,
    distance: BigInt(`0x${parseWebRtcDirectMultiaddr(endpoint).peerId}`) ^ target,
  })).sort((a,b) => a.distance < b.distance ? -1 : 1);
  // Seven misses must not prevent trying a holder already in the bounded
  // candidate set while the ordinary discovery walk is still outstanding.
  const holder = ordered[8].index;
  nodes[holder].chunk = datamap.content;
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader, deadline;
  try {
    reader = await Promise.race([
      client.openPublicFile(datamap.address),
      new Promise((_, reject) => deadline = setTimeout(() => reject(new Error("speculation exhausted before holder")), 1500)),
    ]);
    assert.equal(reader.size, content.length);
    const gets = rtc.requests.filter(r => r.method === "get_chunk");
    assert.ok(gets.some(r => r.node === holder));
    assert.equal(new Set(gets.map(r => r.node)).size, gets.length);
  } finally { clearTimeout(deadline); reader?.close(); client.close(); }
});

test("cold closer hints cannot occupy the read slots before a connected holder", async () => {
  const nodes = [{ view: [1, 2, 3] }, {}, {}, {}];
  const rtc = mockWebRtc(nodes);
  const target = BigInt(`0x${datamap.address}`);
  const ordered = [1, 2, 3].sort((a, b) => {
    const distance = i => BigInt(`0x${parseWebRtcDirectMultiaddr(rtc.endpoints[i]).peerId}`) ^ target;
    return distance(a) < distance(b) ? -1 : 1;
  });
  for (const i of ordered.slice(0, 2)) nodes[i].connectDelay = 2500;
  const holder = ordered[2]; nodes[holder].chunk = datamap.content;
  const client = new BrowserNetworkClient(rtc.endpoints.slice(0, 1));
  let reader, deadline;
  try {
    reader = await Promise.race([
      client.openPublicFile(datamap.address),
      new Promise((_, reject) => deadline = setTimeout(() => reject(new Error("cold hints occupied read slots")), 1500)),
    ]);
    assert.equal(reader.size, content.length);
    assert.ok(rtc.requests.some(r => r.node === holder && r.method === "get_chunk"));
    assert.ok(!rtc.requests.some(r => ordered.slice(0, 2).includes(r.node) && r.method === "get_chunk"));
  } finally { clearTimeout(deadline); reader?.close(); client.close(); }
});

test("shared Client rejects corrupt content immediately, matching native GET", async () => {
  const rtc = mockWebRtc(Array.from({ length: 24 }, () => ({ chunk: new Uint8Array([1, 2, 3]), respond: failDiscovery })));
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    await assert.rejects(client.openPublicFile(datamap.address), /BLAKE3 mismatch/);
    const gets = rtc.requests.filter(request => request.method === "get_chunk");
    // Discovery can finish while the first response yields to the event loop.
    // The shared policy then permits one ordinary GET alongside the early GET.
    assert.ok(gets.length >= 1 && gets.length <= 2, "at most the already-racing GETs may run");
    await new Promise(resolve => setTimeout(resolve, 20));
    assert.equal(rtc.requests.filter(request => request.method === "get_chunk").length, gets.length,
      "integrity failure must cancel the race without fallback or retry");
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

test("native shrunk DataMaps download and stream through the shared async engine", async () => {
  const maxChunkSize = 4_190_208;
  const original = Uint8Array.from({ length: 3 * maxChunkSize + 1 }, (_, index) => index * 37);
  const encrypted = encryptPublicFile(original);
  const rtc = mockWebRtc([{}]);
  for (const record of encrypted.records) rtc.stores[0].set(record.address, record.content);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    const address = encrypted.records.at(-1).address;
    const downloaded = await client.downloadPublicFile(address, 3);
    assert.deepEqual(downloaded.content, original);
    reader = await client.openPublicFile(address);
    const boundary = encrypted.chunks[0].src_size;
    for (const [start, length] of [[0, 32], [boundary - 1, 2], [boundary, 50], [original.length - 10, 100], [original.length, 10], [0, 0]]) {
      assert.deepEqual(await reader.readRange(start, length), original.slice(start, start + length));
    }
    reader.close();
    await assert.rejects(reader.readRange(0, 1), /closed/);
  } finally { reader?.close(); client.close(); }
});

test("omitting the download cap selects the shared adaptive scheduler", async () => {
  const rtc = mockWebRtc([{}]);
  for (const record of encrypted.records) rtc.stores[0].set(record.address, record.content);
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    const result = await client.downloadPublicFile(datamap.address, undefined);
    assert.deepEqual(result.content, content);
  } finally { client.close(); }
});
