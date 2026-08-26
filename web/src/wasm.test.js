import assert from "node:assert/strict";
import test from "node:test";
import {
  BrowserIterativeLookup,
  decodePublicDataMap,
  decryptPublicFile,
  encryptPublicFile,
  verifyRecord,
} from "../pkg/ant_core.js";

const EXPECTED_CHUNK_ADDRESSES = [
  "c024c6884a2f39be7ba07c3d9636efedeb94df7397fcd38bac5ae904643c5cc9",
  "350a88e6eb0b2a3e774107a212a272b4191af69ca4366a4b91f5a1e5872c459a",
  "d73db5a8b0be3b571b40d2b80ff490fe45e135f1992c5863ecb78e25d00ceddb",
];

function lookupNode(lastByte, stringEndpoint = false) {
  return {
    peer_id: `${"00".repeat(31)}${lastByte.toString(16).padStart(2, "0")}`,
    native_addresses: [],
    reliability: 1,
    webrtc_direct: stringEndpoint ? `/test/${lastByte}` : { multiaddr: `/test/${lastByte}` },
  };
}

test("generated WASM drives Saorsa's complete shared iterative lookup", async () => {
  const lookup = new BrowserIterativeLookup("00".repeat(32), 2, 2, 20);
  lookup.addCandidates([lookupNode(3), lookupNode(1), lookupNode(2, true)]);
  const batches = [];
  const termination = await lookup.run(async ({ iteration, candidates }) => {
    batches.push(candidates.map((node) => node.peer_id));
    if (iteration === 1) {
      assert.equal(candidates[1].webrtc_direct, "/test/2");
      return [
        {
          status: "succeeded",
          responder: candidates[0].peer_id,
          candidates: [lookupNode(0)],
        },
        { status: "failed", responder: candidates[1].peer_id },
      ];
    }
    return candidates.map((candidate) => ({
      status: "succeeded",
      responder: candidate.peer_id,
      candidates: [],
    }));
  });

  assert.equal(termination, "Exhausted");
  assert.deepEqual(batches, [
    [lookupNode(1).peer_id, lookupNode(2).peer_id],
    [lookupNode(0).peer_id, lookupNode(3).peer_id],
  ]);
  assert.deepEqual(
    lookup.results().map((node) => node.peer_id),
    [lookupNode(0).peer_id, lookupNode(1).peer_id],
  );
  assert.deepEqual(lookup.queriedPeers(), [
    lookupNode(1).peer_id,
    lookupNode(2).peer_id,
    lookupNode(0).peer_id,
    lookupNode(3).peer_id,
  ]);
});

test("generated ant-core WASM matches the native self-encryption vector", () => {
  const content = new TextEncoder().encode("browser whole-file fixture\n".repeat(160));
  const encrypted = encryptPublicFile(content);

  assert.equal(
    encrypted.address,
    "0d3636dd504d04a236f7e104909234766f077fa7e1ca4a18293d3d168d5f169b",
  );
  assert.equal(
    encrypted.blake3,
    "e0e422267ac59c56bf032d6d830035d343369d20147dd5f6b63351a29b015f22",
  );
  assert.deepEqual(
    encrypted.chunks.map((chunk) => chunk.dst_hash),
    EXPECTED_CHUNK_ADDRESSES,
  );
  assert.equal(encrypted.records.length, 4);

  for (const record of encrypted.records) {
    assert(record.content instanceof Uint8Array);
    assert.equal(verifyRecord(record.address, record.content), record.address);
  }

  const dataMap = encrypted.records.at(-1).content;
  const chunks = encrypted.records.slice(0, -1).map((record) => record.content);
  assert.deepEqual(
    decodePublicDataMap(dataMap).map((chunk) => chunk.dst_hash),
    EXPECTED_CHUNK_ADDRESSES,
  );
  assert.deepEqual(decryptPublicFile(dataMap, chunks), content);

  const tampered = chunks.map((chunk) => chunk.slice());
  tampered[0][0] ^= 1;
  assert.throws(() => decryptPublicFile(dataMap, tampered), /BLAKE3 mismatch/);
});
