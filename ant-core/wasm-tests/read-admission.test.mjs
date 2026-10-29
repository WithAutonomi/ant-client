import assert from "node:assert/strict";
import test from "node:test";
import { test_budgeted_reads, test_get_dispatched_before_peer_slot } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

// GETs the read budget admits at once: 128 MiB of responses at about 15 MiB each.
const readBudgetCapacity = 8;
// An adaptive fetch cap below the read budget's capacity: the fetch channel's
// floor, which it never adapts below.
const fetchCap = 4;

function slowNodes() {
  const physical = { active: 0, peak: 0 };
  const rtc = mockWebRtc(Array.from({ length: 12 }, () => ({
    delay: method => method === "get_chunk" ? 150 : 0,
    respond(_channel, method) {
      if (method === "get_chunk") {
        physical.peak = Math.max(physical.peak, ++physical.active);
        setTimeout(() => physical.active--, 150);
      }
    },
  })));
  return { rtc, physical };
}

test("without a fetch limiter, independent physical reads share the read budget", async () => {
  const { rtc, physical } = slowNodes();
  const results = await test_budgeted_reads(rtc.endpoints, false);
  assert.ok(results.every(Boolean));
  assert.equal(physical.peak, readBudgetCapacity, `physical peak ${physical.peak}`);
  assert.equal(rtc.requests.filter(r => r.method === "get_chunk").length, 12);
  assert.ok(rtc.connections.every(connection => connection.closed));
});

test("independent physical reads stay within the adaptive fetch cap", async () => {
  const { rtc, physical } = slowNodes();
  const results = await test_budgeted_reads(rtc.endpoints, false, fetchCap);
  assert.ok(results.every(Boolean));
  assert.equal(physical.peak, fetchCap, `physical peak ${physical.peak}`);
  assert.equal(rtc.requests.filter(r => r.method === "get_chunk").length, 12);
  assert.ok(rtc.connections.every(connection => connection.closed));
});

test("pool closure cancels both active reads and queued read reservations", async () => {
  const rtc = mockWebRtc(Array.from({ length: 12 }, () => ({ delay: method => method === "get_chunk" ? 200 : 0 })));
  const results = await test_budgeted_reads(rtc.endpoints, true);
  assert.ok(results.every(result => !result));
  assert.ok(rtc.requests.filter(r => r.method === "get_chunk").length <= readBudgetCapacity);
  assert.ok(rtc.connections.every(connection => connection.closed));
});

test("a GET waiting for its peer's session slot has been dispatched", async () => {
  // The node's session carries one request at a time, so the second GET waits
  // for the first one's reply. Its hedge delay must already be running: a busy
  // or silent peer is the peer's delay, not the client's.
  let replies = 0;
  const rtc = mockWebRtc([{
    delay: method => method === "get_chunk" ? 200 : 0,
    delivered(_channel, method) { if (method === "get_chunk") replies++; },
  }]);
  const atDispatch = await test_get_dispatched_before_peer_slot(rtc.endpoints[0], () => replies);
  assert.equal(atDispatch, 0);
  assert.equal(replies, 2);
});
