import assert from "node:assert/strict";
import test from "node:test";
import { test_connect_node, test_pooled_requests, test_admission_deadlines, test_close_pool_during_connect } from "./pkg/ant_core.js";
import { mockWebRtc, paymentNetwork } from "./mock-webrtc.mjs";

async function requests(rtc, { warm = true, payment = undefined, before = () => {} } = {}) {
  const result = await test_pooled_requests(rtc.endpoints[0], 300, warm, payment, before);
  return result.map(value => Object.fromEntries(value));
}

test("each queued RPC receives its full response budget", async () => {
  const rtc = mockWebRtc([{ delay: method => method === "get_chunk" ? 200 : 0 }]);
  const results = await requests(rtc);
  assert.deepEqual(results.map(r => r.result), ["ok", "ok", "ok"]);
  assert.equal(rtc.requests.filter(r => r.method === "get_chunk").length, 3);
  assert.equal(rtc.connections.length, 1);
});

test("send backpressure does not consume the response budget", async () => {
  const rtc = mockWebRtc([{}]);
  const results = await requests(rtc, { before() {
    const channel = rtc.connections[0].channel;
    channel.bufferedAmount = 3 * 1024 * 1024;
    setTimeout(() => { channel.bufferedAmount = 0; channel.onbufferedamountlow?.({}); }, 450);
  } });
  assert.deepEqual(results.map(r => r.result), ["ok", "ok", "ok"]);
  assert.equal(rtc.connections.length, 1);
});

test("an expired send budget is a transfer timeout and retires the sealed association", async () => {
  const rtc = mockWebRtc([{}]);
  const results = await requests(rtc, { before() {
    rtc.connections[0].channel.bufferedAmount = 3 * 1024 * 1024;
  } });
  assert.match(results[0].result, /^timeout: WebRTC request transfer timed out/);
  assert.deepEqual(results.slice(1).map(r => r.result), ["ok", "ok"]);
  assert.equal(rtc.connections.length, 2);
  assert.equal(rtc.connections[0].closed, true);
  assert.equal(rtc.requests.filter(r => r.method === "get_chunk").length, 2);
});

// V2-1305: the timeout closes the session while the mock, like
// node-datachannel, still reports the channel open. The queued RPCs must redial
// rather than retry admission on the dead session, which never yields and so
// blocks the event loop until the 400 s admission deadline.
test("a queued pooled RPC authenticates a replacement after response timeout", async () => {
  let first = true;
  const rtc = mockWebRtc([{ respond(channel, method, response) {
    if (method !== "get_chunk" || !first) return;
    first = false;
    // The abandoned reply must never be consumed by a subsequent RPC.
    setTimeout(() => channel.emit(response.buffer), 450);
    return false;
  } }]);
  const results = await requests(rtc);
  // A redial takes milliseconds after the 300 ms response timeout; a spin
  // holds the queued RPCs until the admission deadline.
  assert.ok(results.every(r => r.finishedMs < 5_000), JSON.stringify(results));
  assert.match(results[0].result, /response.*timed out/);
  assert.deepEqual(results.slice(1).map(r => r.result), ["ok", "ok"]);
  assert.equal(rtc.connections.length, 2);
  assert.equal(rtc.connections[0].closed, true);
  assert.equal(rtc.requests.filter(r => r.method === "hello").length, 2);
});

test("concurrent cold pooled RPCs share one authenticated HELLO", async () => {
  const rtc = mockWebRtc([{ delay: method => method === "hello" ? 100 : 0 }]);
  const results = await requests(rtc, { warm: false });
  assert.deepEqual(results.map(r => r.result), ["ok", "ok", "ok"]);
  assert.equal(rtc.requests.filter(r => r.method === "hello").length, 1);
});

for (const early of [false, true]) test(`final send buffer drain preserves ${early ? "early responses" : "the response budget"}`, async () => {
  let first = true;
  const rtc = mockWebRtc([{ respond(channel, method, response) {
    if (method !== "get_chunk" || !first) return;
    first = false;
    // Below the ordinary high-water mark: even a small final tail must drain.
    channel.bufferedAmount = 16_384;
    setTimeout(() => { channel.bufferedAmount = 0; channel.onbufferedamountlow?.({}); }, 450);
    setTimeout(() => channel.emit(response.buffer), early ? 20 : 650);
    return false;
  } }]);
  const results = await requests(rtc);
  assert.deepEqual(results.map(r => r.result), ["ok", "ok", "ok"]);
  assert.ok(results[0].finishedMs >= 400);
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.connections[0].channel.onbufferedamountlow ?? null, null);
});

test("closure during final drain wakes the sender and queued work reconnects", async () => {
  let first = true;
  const rtc = mockWebRtc([{ respond(channel, method) {
    if (method !== "get_chunk" || !first) return;
    first = false;
    channel.bufferedAmount = 16_384;
    setTimeout(() => channel.close(), 20);
    return false;
  } }]);
  const results = await requests(rtc);
  assert.match(results[0].result, /closed|failed while transmitting/);
  assert.ok(results[0].finishedMs < 250);
  assert.deepEqual(results.slice(1).map(r => r.result), ["ok", "ok"]);
  assert.equal(rtc.connections.length, 2);
});

test("admission expiry preserves the active request and pooled connection", async () => {
  const rtc = mockWebRtc([{ delay: method => method === "find_node" ? 100 : 0 }, {}]);
  await test_admission_deadlines(rtc.endpoints.map(multiaddr => ({ multiaddr })));
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.requests.filter(r => r.method === "find_node").length, 2);
});

test("pool closure during setup cannot publish a replacement association", async () => {
  const rtc = mockWebRtc([{ connectDelay: 50 }]);
  await test_close_pool_during_connect(rtc.endpoints[0]);
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.connections[0].closed, true);
  assert.equal(rtc.requests.filter(r => r.method === "hello").length, 0);
});

test("queued transport generation handles stay closed after their active RPC fails", async () => {
  const rtc = mockWebRtc([{ respond(channel, method) {
    if (method !== "find_node") return;
    setTimeout(() => channel.close(), 20);
    return false;
  } }]);
  const session = await test_connect_node(rtc.endpoints[0]);
  try {
    const results = await Promise.allSettled([
      session.findNode("11".repeat(32), 20),
      session.findNode("22".repeat(32), 20),
      session.hello(),
    ]);
    assert.ok(results.every(r => r.status === "rejected"));
    assert.match(String(results[1].reason), /session closed/);
    assert.match(String(results[2].reason), /session closed/);
    assert.equal(rtc.connections.length, 1);
    assert.equal(rtc.requests.filter(r => r.method === "find_node").length, 1);
  } finally { session.close(); session.free(); }
});


test("a replacement connection must still advertise the required upload capabilities", async () => {
  let first = true;
  const options = [{ respond(_channel, method) {
    if (method !== "quote_chunk" || !first) return;
    first = false;
    options[0].uploads = false;
    return false;
  } }];
  const rtc = mockWebRtc(options);
  const results = await requests(rtc, { payment: paymentNetwork });
  assert.match(results[0].result, /response.*timed out/);
  assert.ok(results.slice(1).every(r => /does not advertise paid browser uploads/.test(r.result)));
  assert.equal(rtc.requests.filter(r => r.method === "quote_chunk").length, 1);
  assert.equal(rtc.connections.length, 2);
});
