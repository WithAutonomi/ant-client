import assert from "node:assert/strict";
import test from "node:test";
import { test_abandoned_lookup, test_abandoned_queued_lookup, test_preconnect_pool } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

for (const phase of ["setup", "handshake", "hello", "find_node"]) {
  test(`abandoned lookup during ${phase} drains and reuses the authenticated connection`, async () => {
    const rtc = mockWebRtc([{
      connectDelay: phase === "setup" ? 70 : 0,
      delay: method => method === phase ? 70 : 0,
    }]);
    assert.equal(await test_abandoned_lookup(rtc.endpoints[0], false), "ok");
    assert.equal(rtc.connections.length, 1);
    assert.equal(rtc.requests.filter(r => r.method === "hello").length, 1);
    assert.equal(rtc.requests.filter(r => r.method === "find_node").length, 2);
    assert.ok(rtc.connections.every(c => c.closed));
  });
}

test("a real dial failure after lookup cancellation is retained for the next lookup", async () => {
  const rtc = mockWebRtc([{ connectErrorDelay: 70, connectError: "unreachable endpoint" }]);
  assert.match(await test_abandoned_lookup(rtc.endpoints[0], false), /failed-connection cache/);
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.connections[0].closed, true);
});

for (const phase of ["setup", "find_node"]) {
  test(`pool closure aborts an abandoned lookup during ${phase}`, async () => {
    const rtc = mockWebRtc([{
      connectDelay: phase === "setup" ? 70 : 0,
      delay: method => method === phase ? 70 : 0,
    }]);
    assert.match(await test_abandoned_lookup(rtc.endpoints[0], true), /pool is closed/);
    assert.equal(rtc.connections.length, 1);
    assert.equal(rtc.connections[0].closed, true);
    assert.equal(rtc.requests.filter(r => r.method === "find_node").length, phase === "setup" ? 0 : 1);
  });
}

test("an abandoned queued lookup sends no request and leaves the active RPC intact", async () => {
  const rtc = mockWebRtc([{ delay: method => method === "find_node" ? 70 : 0 }]);
  await test_abandoned_queued_lookup(rtc.endpoints[0]);
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.requests.filter(r => r.method === "find_node").length, 1);
});

for (const capacity of [3, 64]) {
  test(`preconnections are deduplicated and bounded with pool capacity ${capacity}`, async () => {
    const rtc = mockWebRtc(Array.from({ length: 12 }, () => ({ connectDelay: 70 })));
    await test_preconnect_pool(rtc.endpoints, true, false, capacity);
    assert.equal(rtc.connections.length, Math.min(capacity, 8));
    assert.equal(new Set(rtc.connections.map(c => c.channel.index)).size, rtc.connections.length);
    assert.equal(rtc.requests.filter(r => r.method === "hello").length, rtc.connections.length);
    assert.ok(rtc.connections.every(c => c.closed));
  });
}

test("unsigned discovery hints do not trigger speculative connections", async () => {
  const rtc = mockWebRtc(Array.from({ length: 12 }, () => ({})));
  await test_preconnect_pool(rtc.endpoints, false, false, 64);
  assert.equal(rtc.connections.length, 0);
});

test("pool closure promptly retires speculative connection setups", async () => {
  const rtc = mockWebRtc(Array.from({ length: 12 }, () => ({ connectDelay: 70 })));
  await test_preconnect_pool(rtc.endpoints, true, true, 64);
  assert.equal(rtc.connections.length, 8);
  assert.ok(rtc.connections.every(c => c.closed));
  assert.equal(rtc.requests.filter(r => r.method === "hello").length, 0);
});
