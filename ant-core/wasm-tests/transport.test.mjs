import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNodeClient, contentAddress } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

test("idle pooled connections reject unsolicited messages immediately", async () => {
  const rtc = mockWebRtc();
  const client = new BrowserNodeClient(rtc.endpoints[0]);
  try {
    await client.hello();
    const connection = rtc.connections[0];
    for (let i = 0; i < 256; i++) {
      connection.channel.emit(new Uint8Array(16_384).buffer);
    }
    assert.equal(connection.closed, true);
    // The failed association must not poison subsequent requests.
    await client.hello();
    assert.equal(rtc.connections.length, 2);
  } finally {
    client.close();
  }
});

for (const scenario of ["oversized", "byte budget", "tiny messages", "empty"]) {
  test(`active response rejects ${scenario} before retaining unbounded data`, async () => {
    const rtc = mockWebRtc([{
      respond(channel, method) {
        if (method !== "get_chunk") return;
        if (scenario === "oversized") {
          channel.emit(new Uint8Array(5 * 1024 * 1024).buffer);
        } else if (scenario === "empty") {
          channel.emit(new ArrayBuffer(0));
        } else {
          const fragment = new Uint8Array(scenario === "byte budget" ? 16_384 : 1);
          for (let i = 0; i < 600; i++) channel.emit(fragment.buffer);
        }
        return false;
      },
    }]);
    const client = new BrowserNodeClient(rtc.endpoints[0]);
    try {
      await client.hello();
      await assert.rejects(
        client.getChunk("11".repeat(32)),
        /message size|byte budget|too many messages/,
      );
      assert.equal(rtc.connections[0].closed, true);
    } finally {
      client.close();
    }
  });
}

test("fragmented authenticated responses preserve normal connection reuse", async () => {
  const chunk = new Uint8Array(4 * 1024 * 1024).fill(0xab);
  const rtc = mockWebRtc([{ chunk }]);
  const client = new BrowserNodeClient(rtc.endpoints[0]);
  try {
    await client.hello();
    const result = await client.getChunk(contentAddress(chunk));
    assert.deepEqual(result.content, chunk);
    await client.findNode("11".repeat(32), 20);
    assert.equal(rtc.connections.length, 1);
    assert.notEqual(rtc.connections[0].closed, true);
  } finally {
    client.close();
  }
});

for (const message of ["price too low", "ICE failure", "remote request timed out"]) {
  test(`authenticated PUT rejection stays an application failure: ${message}`, async () => {
    const { test_put_failure_kind } = await import("./pkg/ant_core.js");
    const rtc = mockWebRtc([{ putError: { code: "put_failed", message } }]);
    assert.equal(await test_put_failure_kind(rtc.endpoints[0]), "Application");
  });
}

test("an actual PUT response deadline remains a network capacity signal", async () => {
  const { test_put_failure_kind } = await import("./pkg/ant_core.js");
  const rtc = mockWebRtc([{ respond: (_, method) => method !== "put_chunk" }]);
  assert.equal(await test_put_failure_kind(rtc.endpoints[0]), "Network");
});
