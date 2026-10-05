import assert from "node:assert/strict";
import test from "node:test";
import { test_preconnect_pool } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

// The transport passes the turn on after polling for completion every 10 ms,
// and gives up on a gathering that has not completed after 1 s.
const TURN_TIMEOUT_MS = 1_000;
const TIMER_SLACK_MS = 50;

const byStart = spans => [...spans].sort((a, b) => a.start - b.start);

test("concurrent dials gather ICE candidates one connection at a time", async () => {
  const rtc = mockWebRtc(Array.from({ length: 12 }, () => ({})), { gatheringMs: 30 });
  await test_preconnect_pool(rtc.endpoints, true, false, 64);
  const spans = byStart(rtc.gathering);
  assert.equal(spans.length, 8);
  for (let i = 1; i < spans.length; i++) {
    assert.ok(spans[i].start >= spans[i - 1].complete,
      `gathering ${i} started at ${spans[i].start.toFixed(1)} ms, before ${i - 1} completed at ${spans[i - 1].complete.toFixed(1)} ms`);
  }
});

test("a gathering that never completes passes the turn on after the timeout", async () => {
  const rtc = mockWebRtc(Array.from({ length: 2 }, () => ({})), { gatheringMs: Infinity });
  await test_preconnect_pool(rtc.endpoints, true, false, 64);
  const spans = byStart(rtc.gathering);
  assert.equal(spans.length, 2);
  const waited = spans[1].start - spans[0].start;
  assert.ok(waited >= TURN_TIMEOUT_MS - TIMER_SLACK_MS && waited < TURN_TIMEOUT_MS + 10 * TIMER_SLACK_MS, `waited ${waited.toFixed(0)} ms`);
  assert.equal(rtc.requests.filter(r => r.method === "hello").length, 2);
});
