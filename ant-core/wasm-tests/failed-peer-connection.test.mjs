import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { test_connect_node } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

// Well inside the 10 s first-response deadline a dead association used to cost.
const PROMPT_MS = 2_000;

test("a request fails at once when its peer connection fails with its channels still open", async () => {
  const rtc = mockWebRtc([{ respond(channel, method) {
    if (method !== "get_chunk") return;
    channel.connection.failLikeWebKit();
    return false;
  } }]);
  const client = await test_connect_node(rtc.endpoints[0]);
  try {
    await client.hello();
    const started = Date.now();
    await assert.rejects(client.getChunk("11".repeat(32)), /peer connection failed/);
    assert.ok(Date.now() - started < PROMPT_MS, `took ${Date.now() - started} ms`);
    assert.equal(rtc.connections[0].channel.readyState, "open");
  } finally {
    client.close();
  }
});

test("a pooled association that failed with its channels still open is redialled, not reused", async () => {
  const rtc = mockWebRtc([{}]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    await client.findClosest("00".repeat(32));
    assert.equal(rtc.connections.length, 1);
    const failed = rtc.connections[0];
    const lanes = failed.channels.length;
    failed.failLikeWebKit();

    const started = Date.now();
    const result = await client.findClosest("00".repeat(32));
    assert.ok(result.nodes.length > 0);
    assert.ok(Date.now() - started < PROMPT_MS, `took ${Date.now() - started} ms`);
    assert.equal(rtc.connections.length, 2);
    assert.equal(failed.channels.length, lanes, "no lane may be opened on the failed association");
  } finally {
    client.close();
  }
});
