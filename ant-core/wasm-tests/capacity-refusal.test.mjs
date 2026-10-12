import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { mockWebRtc } from "./mock-webrtc.mjs";

// Well inside the 10 s channel-open deadline an unanswered dial costs.
const PROMPT_MS = 2_000;
// The client's ENDPOINT_REFUSAL_COOLDOWN plus scheduling margin. An endpoint
// that never answered stays suppressed for 30 minutes instead.
const AFTER_REFUSAL_COOLDOWN_MS = 15_500;

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

test("a node that declines a dial fails it at once and is dialled again after the refusal cooldown", async () => {
  // One node refuses at its transport (a STUN error), the other at its
  // application (it closes the association during the handshake).
  const rtc = mockWebRtc([{ refuseDials: 1 }, { closeHandshakes: 1 }]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    const started = Date.now();
    await assert.rejects(client.connect());
    assert.ok(Date.now() - started < PROMPT_MS, `took ${Date.now() - started} ms`);
    assert.equal(rtc.connections.length, 2);

    // Inside the cooldown neither node is dialled.
    await assert.rejects(client.connect());
    assert.equal(rtc.connections.length, 2);

    await sleep(AFTER_REFUSAL_COOLDOWN_MS);
    const hello = await client.connect();
    assert.equal(hello.type, "hello");
    assert.ok(rtc.connections.length > 2);
  } finally {
    client.close();
  }
});
