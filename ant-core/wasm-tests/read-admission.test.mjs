import assert from "node:assert/strict";
import test from "node:test";
import { test_budgeted_reads } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

test("independent physical reads share the adaptive admission budget", async () => {
  let active = 0, peak = 0;
  const rtc = mockWebRtc(Array.from({ length: 12 }, () => ({
    delay: method => method === "get_chunk" ? 150 : 0,
    respond(_channel, method) {
      if (method === "get_chunk") {
        peak = Math.max(peak, ++active);
        setTimeout(() => active--, 150);
      }
    },
  })));
  const results = await test_budgeted_reads(rtc.endpoints, false);
  assert.ok(results.every(Boolean));
  assert.ok(peak > 1 && peak <= 4, `physical peak ${peak}`);
  assert.equal(rtc.requests.filter(r => r.method === "get_chunk").length, 12);
  assert.ok(rtc.connections.every(connection => connection.closed));
});

test("pool closure cancels both active reads and queued read reservations", async () => {
  const rtc = mockWebRtc(Array.from({ length: 12 }, () => ({ delay: method => method === "get_chunk" ? 200 : 0 })));
  const results = await test_budgeted_reads(rtc.endpoints, true);
  assert.ok(results.every(result => !result));
  assert.ok(rtc.requests.filter(r => r.method === "get_chunk").length <= 4);
  assert.ok(rtc.connections.every(connection => connection.closed));
});
