import assert from "node:assert/strict";
import test from "node:test";
import { test_parallel_lanes, test_active_data_lane_capacity, test_redial_after_local_close, test_concurrent_failed_lanes } from "./pkg/ant_core.js";
import { closesSettled, mockWebRtc } from "./mock-webrtc.mjs";
const object = value => value instanceof Map ? Object.fromEntries([...value].map(([k,v]) => [k, object(v)])) : value;

test("a failed cold dial is shared by control and data waiters", async () => {
  const rtc = mockWebRtc([{ connectErrorDelay: 70, connectError: "unreachable endpoint" }]);
  const results = await test_concurrent_failed_lanes(rtc.endpoints[0]);
  assert.match(results[0], /unreachable endpoint/);
  assert.match(results[1], /failed-connection cache/);
  assert.equal(rtc.connections.length, 1, "the queued lane must not redial after the first lane failed");
  assert.ok(rtc.connections.every(c => c.closed));
});

for (const slow of ["find_node", "get_chunk"]) {
  test(`a slow ${slow} does not block the other authenticated lane`, async () => {
    const rtc = mockWebRtc([{ delay: method => method === slow ? 200 : 0 }]);
    const result = object(await test_parallel_lanes(rtc.endpoints[0], "none"));
    const fast = slow === "find_node" ? result.data : result.control;
    const delayed = slow === "find_node" ? result.control : result.data;
    assert.equal(fast.result, "ok");
    assert.equal(delayed.result, "ok");
    assert.ok(fast.ms < 150, JSON.stringify(result));
    assert.ok(delayed.ms >= 190, JSON.stringify(result));
    assert.equal(rtc.connections.length, 1);
    assert.equal(rtc.connections[0].channels.length, 2);
    assert.equal(rtc.requests.filter(r => r.method === "hello").length, 2);
    assert.ok(rtc.connections.every(c => c.closed));
  });
}

for (const lane of ["control", "data"]) {
  test(`cancelling the ${lane} lane preserves the other session and reuses ICE`, async () => {
    const slow = lane === "control" ? "find_node" : "get_chunk";
    const rtc = mockWebRtc([{ delay: method => method === slow ? 70 : 0 }]);
    const result = object(await test_parallel_lanes(rtc.endpoints[0], lane));
    assert.equal(result[lane].result, "cancelled");
    assert.equal(result[lane === "control" ? "data" : "control"].result, "ok");
    assert.equal(rtc.connections.length, 1);
    assert.equal(rtc.connections[0].channels.length, 2);
    assert.equal(rtc.requests.filter(r => r.method === "hello").length, 2);
    await closesSettled();
    assert.ok(rtc.connections[0].channels.every(c => c.readyState === "closed"));
  });
}

test("malformed bulk ingress does not invalidate the control channel", async () => {
  let first = true;
  const rtc = mockWebRtc([{ respond(channel, method) {
    if (method === "get_chunk" && first) { first = false; channel.emit(new ArrayBuffer(0)); return false; }
  } }]);
  const result = object(await test_parallel_lanes(rtc.endpoints[0], "none"));
  assert.match(result.data.result, /invalid WebRTC response message size/);
  assert.equal(result.control.result, "ok");
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.connections[0].channels.length, 3);
});

test("closing a pool wakes both active lanes and releases the association", async () => {
  const rtc = mockWebRtc([{ delay: method => ["find_node", "get_chunk"].includes(method) ? 200 : 0 }]);
  const result = object(await test_parallel_lanes(rtc.endpoints[0], "close"));
  assert.match(result.control.result, /closed/);
  assert.match(result.data.result, /closed/);
  assert.ok(result.control.ms < 150 && result.data.ms < 150);
  assert.equal(rtc.connections.length, 1);
  await closesSettled();
  assert.ok(rtc.connections[0].channels.every(c => c.readyState === "closed"));
});

test("a lane closed locally redials a fresh association rather than reusing its own", async () => {
  const rtc = mockWebRtc([{}]);
  await test_redial_after_local_close(rtc.endpoints[0]);
  // The retired channel still read "open" when the redial began.
  assert.equal(rtc.connections.length, 2);
  assert.equal(rtc.connections[0].channels.length, 1);
});

test("an active data lease cannot be evicted by another peer", async () => {
  const rtc = mockWebRtc([{}, {}]);
  await test_active_data_lane_capacity(rtc.endpoints);
  assert.equal(rtc.connections.length, 2);
  assert.ok(rtc.connections.every(c => c.closed));
});

test("concurrent cold control and bulk work share one ICE association", async () => {
  const rtc = mockWebRtc([{ connectDelay: 50 }]);
  const result = object(await test_parallel_lanes(rtc.endpoints[0], "cold"));
  assert.equal(result.control.result, "ok");
  assert.equal(result.data.result, "ok");
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.connections[0].channels.length, 2);
  assert.equal(rtc.requests.filter(r => r.method === "hello").length, 2);
});
