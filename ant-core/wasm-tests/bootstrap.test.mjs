import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient, encryptPublicFile, parseWebRtcDirectMultiaddr } from "./pkg/ant_core.js";
import { mockWebRtc, closesSettled, paymentNetwork } from "./mock-webrtc.mjs";

const content = new TextEncoder().encode("Pooled bootstrap and progressive discovery.".repeat(100));
const map = encryptPublicFile(content).records.at(-1);
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

test("network connect returns a fast seed and retains its authenticated association", async () => {
  const rtc = mockWebRtc([{ connectDelay: 800 }, { chunk: map.content }]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    const started = performance.now();
    const hello = await client.connect(paymentNetwork);
    assert.equal(hello.peer_id, parseWebRtcDirectMultiaddr(rtc.endpoints[1]).peerId);
    reader = await client.openPublicFile(map.address);
    assert.equal(reader.size, content.length);
    assert.ok(performance.now() - started < 600, "a slow seed must not delay authentication or the read");
    assert.equal(rtc.connections.filter(c => c.remoteSeed === 2).length, 1, "the read must reuse the authenticated bootstrap association");
  } finally { reader?.close(); reader?.free(); client.close(); client.free(); }
  await closesSettled();
  assert.ok(rtc.connections.every(c => c.closed));
});

test("cold discovery reaches a non-seed holder before the slow seed authenticates", async () => {
  const rtc = mockWebRtc([{ view: [0, 2] }, { connectDelay: 800 }, { chunk: map.content, view: [0, 2] }]);
  const client = new BrowserNetworkClient(rtc.endpoints.slice(0, 2));
  let reader;
  try {
    const started = performance.now();
    reader = await client.openPublicFile(map.address);
    assert.equal(reader.size, content.length);
    assert.ok(performance.now() - started < 600, "discovery must start before all seeds authenticate");
    assert.ok(rtc.requests.some(r => r.node === 2 && r.method === "get_chunk"));
  } finally { reader?.close(); reader?.free(); client.close(); client.free(); }
});

test("an exhausted fast seed still falls back through a later seed", async () => {
  const rtc = mockWebRtc([{ view: [0] }, { connectDelay: 150, view: [1, 2] }, { view: [1, 2], chunk: map.content }]);
  const client = new BrowserNetworkClient(rtc.endpoints.slice(0, 2));
  let reader;
  try {
    reader = await client.openPublicFile(map.address);
    assert.equal(reader.size, content.length);
    assert.ok(rtc.requests.some(r => r.node === 1 && r.method === "find_node"));
    assert.ok(rtc.requests.some(r => r.node === 2 && r.method === "get_chunk"));
  } finally { reader?.close(); reader?.free(); client.close(); client.free(); }
});

test("bootstrap admits at most four pending connections and close cancels them", async () => {
  const rtc = mockWebRtc(Array.from({ length: 9 }, () => ({ connectDelay: 500 })));
  const client = new BrowserNetworkClient(rtc.endpoints);
  const connecting = client.connect();
  const rejected = assert.rejects(connecting, /closed/);
  await sleep(50);
  assert.equal(rtc.connections.length, 4);
  client.close();
  await rejected;
  client.free();
  await closesSettled();
  assert.ok(rtc.connections.every(c => c.closed));
  await sleep(550);
  assert.equal(rtc.connections.length, 4, "closing must stop queued seeds from dialing");
});

test("bootstrap rejects the wrong network and cannot change trust policy after startup", async () => {
  const rtc = mockWebRtc([{}, {}]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    await assert.rejects(client.connect({ ...paymentNetwork, chain_id: 1 }), /NETWORK_MISMATCH/);
    await assert.rejects(client.connect(paymentNetwork), /policy cannot change/);
    await assert.rejects(client.openPublicFile(map.address));
    assert.equal(rtc.requests.filter(r => r.method === "get_chunk").length, 0);
    assert.equal(rtc.connections.length, 2);
  } finally { client.close(); client.free(); }
});

test("a fast wrong-network seed cannot win the race or re-enter through discovery", async () => {
  const rtc = mockWebRtc([{ payment: { ...paymentNetwork, chain_id: 1 } }, { connectDelay: 50, chunk: map.content }]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    const hello = await client.connect(paymentNetwork);
    assert.equal(hello.peer_id, parseWebRtcDirectMultiaddr(rtc.endpoints[1]).peerId);
    reader = await client.openPublicFile(map.address);
    assert.equal(reader.size, content.length);
    assert.equal(rtc.requests.filter(r => r.node === 0 && !["handshake", "hello"].includes(r.method)).length, 0);
  } finally { reader?.close(); reader?.free(); client.close(); client.free(); }
});
