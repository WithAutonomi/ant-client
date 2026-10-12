import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { mockWebRtc } from "./mock-webrtc.mjs";

// Well inside the 10 s channel-open deadline an unanswered dial costs.
const PROMPT_MS = 2_000;
// The client's ENDPOINT_REFUSAL_COOLDOWN plus scheduling margin. An endpoint
// that never answered stays suppressed for 30 minutes instead.
const AFTER_REFUSAL_COOLDOWN_MS = 15_500;
// Long enough for every dial started by one connect() to reach the node, since
// dials take turns at ICE gathering.
const DIALS_SETTLED_MS = 500;

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

test("a node that declines a dial fails it at once and is dialled again after the refusal cooldown", async () => {
  // One node refuses at its transport (a STUN error), another at its
  // application (it closes the association during the handshake). A third
  // fails DTLS after ICE connected, as a different node at its address
  // would, and keeps the full failure cooldown.
  const rtc = mockWebRtc([{ refuseDials: 1 }, { closeHandshakes: 1 }, { failDtlsDials: 1 }]);
  const dialsTo = seed => rtc.connections.filter(connection => connection.remoteSeed === seed).length;
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    const started = Date.now();
    await assert.rejects(client.connect());
    assert.ok(Date.now() - started < PROMPT_MS, `took ${Date.now() - started} ms`);
    assert.equal(rtc.connections.length, 3);

    // Inside the cooldown no node is dialled.
    await assert.rejects(client.connect());
    assert.equal(rtc.connections.length, 3);

    await sleep(AFTER_REFUSAL_COOLDOWN_MS);
    const hello = await client.connect();
    assert.equal(hello.type, "hello");
    await sleep(DIALS_SETTLED_MS);
    assert.ok(dialsTo(1) > 1 && dialsTo(2) > 1, "refusing nodes are dialled again");
    assert.equal(dialsTo(3), 1, "a DTLS failure keeps the 30-minute cooldown");
  } finally {
    client.close();
  }
});
