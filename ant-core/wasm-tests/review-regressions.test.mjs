import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { BrowserNetworkClient as RawNetwork, BrowserNodeClient, contentAddress, encryptPublicFile, test_pool_waiters, test_operation_timeout } from "./pkg/ant_core.js";
import { mockWebRtc, paymentNetwork } from "./mock-webrtc.mjs";
const content = new TextEncoder().encode("Recovery regression ".repeat(100));
const receipt = quotes => ({ transactionHash: `0x${"ab".repeat(32)}`, totalAmount: quotes.reduce((sum, quote) => sum + BigInt(quote.amount), 0n).toString() });

for (const malformed of ["amount", "hash"]) test(`invalid wallet ${malformed} preserves evidence and cannot automatically pay twice`, async () => {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({})));
  let client = new BrowserNetworkClient(rtc.endpoints), checkpoint, calls = 0, recovered = 0;
  const pay = async (_, quotes, submitted) => {
    calls++;
    await submitted(receipt(quotes));
    return { ...receipt(quotes), ...(malformed === "amount" ? { totalAmount: "0" } : { transactionHash: "invalid" }) };
  };
  try {
    await assert.rejects(client.uploadPublicFile(content, "a", "text/plain", paymentNetwork, pay, undefined, undefined, value => { checkpoint = value; }));
    client.close(); client = new BrowserNetworkClient(rtc.endpoints);
    await assert.rejects(client.uploadPublicFile(content, "a", "text/plain", paymentNetwork, pay, undefined, checkpoint), /outcome unknown/);
    assert.equal(calls, 1);
    pay.recover = async (_, quotes, _submitted, attempt) => {
      recovered++; assert.equal(attempt.submissions[0].transactionHash, receipt(quotes).transactionHash);
      assert.equal(attempt.receipt[malformed === "amount" ? "totalAmount" : "transactionHash"], malformed === "amount" ? "0" : "invalid");
      return receipt(quotes);
    };
    await client.uploadPublicFile(content, "a", "text/plain", paymentNetwork, pay, undefined, checkpoint);
    assert.equal(calls, 1); assert.equal(recovered, 1);
  } finally { client.close(); }
});

test("paid low-level uploads require checkpoint persistence before invoking the wallet", async () => {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({})));
  const client = new RawNetwork(rtc.endpoints); let calls = 0;
  try {
    await assert.rejects(client.uploadPublicFile(content, "a", "text/plain", paymentNetwork, async (_, quotes) => { calls++; return receipt(quotes); }), /checkpoint persistence/);
    assert.equal(calls, 0);
  } finally { client.close(); }
});

test("staged bytes are preflighted and descriptor fields are derived from the DataMap", async () => {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({})));
  const client = new BrowserNetworkClient(rtc.endpoints), encrypted = encryptPublicFile(content);
  const staged = { address: encrypted.address, name: "a", content_type: "text/plain", size: 99, chunks: [], blake3: "invalid hint",
    records: encrypted.records.map(record => ({ address: record.address, size: record.content.length })) };
  let calls = 0;
  const pay = async (_, quotes) => { calls++; return receipt(quotes); };
  try {
    await assert.rejects(client.uploadStagedPublicFile(staged, paymentNetwork, index => {
      if (index === 0) throw new Error("record evicted"); return encrypted.records[index].content;
    }, pay), /record evicted/);
    assert.equal(calls, 0);
    const result = await client.uploadStagedPublicFile(staged, paymentNetwork, index => encrypted.records[index].content, pay);
    assert.equal(result.file.size, content.length); assert.equal(result.file.blake3, "");
    const downloaded = await client.downloadPublicFile({ address: result.file.address, name: "a", content_type: "text/plain" }, 3);
    assert.deepEqual(downloaded.content, content); assert.equal(downloaded.hash, contentAddress(content));
  } finally { client.close(); }
});

test("dead discovered candidates never starve the authenticated seed cache", async () => {
  const options = [{ peers: [] }], rtc = mockWebRtc(options), client = new BrowserNetworkClient(rtc.endpoints);
  const prefix = rtc.endpoints[0].slice(0, rtc.endpoints[0].lastIndexOf("/p2p/") + 5);
  try {
    for (let round = 0; round < 16; round++) {
      options[0].peers = Array.from({ length: 20 }, (_, i) => {
        const peer = (round * 20 + i + 1).toString(16).padStart(64, "0");
        return { peer_id: peer, native_addresses: [], reliability: 1, webrtc_direct: { multiaddr: prefix + peer } };
      });
      for (const connection of rtc.connections) if (connection.channel?.index === 0) connection.channel.server?.set_closest_peers(options[0].peers);
      const previous = rtc.requests.filter(request => request.method === "find_node" && request.node === 0).length;
      const result = await client.findClosest("00".repeat(32));
      assert.ok(result.nodes.length > 0);
      assert.ok(rtc.requests.filter(request => request.method === "find_node" && request.node === 0).length > previous);
    }
  } finally { client.close(); }
});

test("wall clock jumps cannot interrupt a progressing response", async () => {
  const bytes = new Uint8Array(64000).fill(42), clock = Date.now;
  const rtc = mockWebRtc([{ chunk: bytes, respond(channel, method, response) {
    if (method !== "get_chunk") return;
    setTimeout(() => {
      channel.emit(response.slice(0, 16384).buffer); Date.now = () => clock() + 60000;
      setTimeout(() => { for (let p = 16384; p < response.length; p += 16384) channel.emit(response.slice(p, p + 16384).buffer); }, 20);
    }, 0);
    return false;
  } }]);
  const client = await new BrowserNodeClient(rtc.endpoints[0]).connect();
  try { assert.deepEqual((await client.getChunk(contentAddress(bytes))).content, bytes); }
  finally { Date.now = clock; client.close(); }
});

test("pool wakes all capacity waiters and closes cleanly after cancellation", async () => {
  const rtc = mockWebRtc(Array.from({ length: 4 }, () => ({})));
  await test_pool_waiters(rtc.endpoints.map(multiaddr => ({ multiaddr })));
});

test("production adapter honors an operation timeout longer than the generic RPC default", async () => {
  const rtc = mockWebRtc([{ respond(channel, method, response) {
    if (method !== "get_chunk") return;
    setTimeout(() => channel.emit(response.buffer), 10500);
    return false;
  } }]);
  await test_operation_timeout(rtc.endpoints[0], 15000);
});
