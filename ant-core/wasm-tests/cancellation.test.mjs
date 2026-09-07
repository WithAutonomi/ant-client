import assert from "node:assert/strict";
import test from "node:test";
import { test_cancel_request, test_cancel_queued_request } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

for (const phase of ["response", "partial response", "send buffer"]) {
  test(`cancellation during ${phase} closes the association before reuse`, async () => {
    let partialSent = false;
    const rtc = mockWebRtc([{
      delay: (method) => method === "find_node" ? 50 : 0,
      respond(channel, method, response) {
        if (phase !== "partial response" || method !== "find_node" || partialSent) return;
        partialSent = true;
        channel.emit(response.slice(0, 8).buffer);
        setTimeout(() => channel.emit(response.slice(8).buffer), 50);
        return false;
      },
    }]);
    await test_cancel_request(rtc.endpoints[0], () => {
      if (phase === "send buffer") rtc.connections[0].channel.bufferedAmount = 3 * 1024 * 1024;
    });
    assert.equal(rtc.connections.length, 2);
    assert.equal(rtc.connections[0].closed, true);
    assert.equal(rtc.connections[0].channel.onbufferedamountlow ?? null, null);
  });
}

test("canceling a lock waiter preserves the active request and session", async () => {
  const rtc = mockWebRtc([{
    delay: (method) => method === "find_node" ? 50 : 0,
  }]);
  await test_cancel_queued_request(rtc.endpoints[0]);
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.requests.filter(({ method }) => method === "find_node").length, 2);
});
