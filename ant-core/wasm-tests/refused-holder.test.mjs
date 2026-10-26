import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { encryptPublicFile } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

const content = new TextEncoder().encode("A holder that is full for now.".repeat(100));
const datamap = encryptPublicFile(content).records.at(-1);

// The client's ENDPOINT_REFUSAL_COOLDOWN.
const REFUSAL_COOLDOWN_MS = 15_000;
// The read engine's MAX_REFUSAL_WAITS.
const MAX_REFUSAL_WAITS = 3;
// Time for the lookup and reads around each wait.
const ROUND_MARGIN_MS = 5_000;

// The seed holds nothing and points at the only holder, which declines its
// first dials as a full node does. The holder stays out of discovery and reads
// for the refusal cooldown, so on its own the seed's answer cannot prove the
// record absent.
function holderRefusing(dials) {
  const rtc = mockWebRtc([{ view: [0, 1] }, { chunk: datamap.content, refuseDials: dials }]);
  return { rtc, client: new BrowserNetworkClient(rtc.endpoints.slice(0, 1)) };
}

test("a read waits out a holder's refusal instead of reporting the record not found", async () => {
  const { rtc, client } = holderRefusing(1);
  let reader;
  try {
    const started = Date.now();
    reader = await client.openPublicFile(datamap.address);
    const elapsed = Date.now() - started;
    assert.equal(reader.size, content.length);
    assert.ok(elapsed >= REFUSAL_COOLDOWN_MS && elapsed < REFUSAL_COOLDOWN_MS + ROUND_MARGIN_MS,
      `took ${elapsed} ms`);
    assert.ok(rtc.requests.some(request => request.node === 1 && request.method === "get_chunk"));
  } finally {
    reader?.close();
    client.close();
  }
});

test("a read whose holder keeps refusing reports it busy, not absent", async () => {
  const { client } = holderRefusing(Infinity);
  try {
    const started = Date.now();
    await assert.rejects(client.openPublicFile(datamap.address), error => {
      const message = String(error?.message ?? error);
      assert.match(message, /busy/);
      assert.doesNotMatch(message, /not found/);
      return true;
    });
    const elapsed = Date.now() - started;
    const waited = MAX_REFUSAL_WAITS * REFUSAL_COOLDOWN_MS;
    assert.ok(elapsed >= waited && elapsed < waited + MAX_REFUSAL_WAITS * ROUND_MARGIN_MS,
      `took ${elapsed} ms`);
  } finally {
    client.close();
  }
});
