import assert from "node:assert/strict";
import test from "node:test";
import {
  BrowserFileEncryptor,
  BrowserIterativeLookup,
  decodePublicDataMap,
  decryptPublicFile,
  encryptPublicFile,
  verifyRecord,
} from "./pkg/ant_core.js";

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
  assert.throws(
    () => decryptPublicFile(dataMap, tampered),
    /record may be missing or corrupt/,
  );
});

test("generated ant-core WASM supports nested DataMaps", () => {
  const maxChunkSize = 4_190_208;
  const content = new Uint8Array(3 * maxChunkSize + 1);
  for (let index = 0; index < content.length; index += 1) {
    content[index] = index;
  }

  const encrypted = encryptPublicFile(content);
  assert.equal(encrypted.chunks.length, 4);
  assert.equal(decodePublicDataMap(encrypted.records.at(-1).content).length, 3);
  assert(encrypted.records.length > encrypted.chunks.length + 1);

  const decrypted = decryptPublicFile(
    encrypted.records.at(-1).content,
    encrypted.records.slice(0, -1).map((record) => record.content),
  );
  assert.equal(decrypted.length, content.length);
  assert.equal(verifyRecord(encrypted.blake3, decrypted), encrypted.blake3);
});

test("streaming WASM encryption emits one externally stageable record at a time", () => {
  const maxChunkSize = 4_190_208;
  const content = new Uint8Array(3 * maxChunkSize + 1);
  for (let index = 0; index < content.length; index += 1) {
    content[index] = index;
  }

  let reads = 0;
  let largestRead = 0;
  const encryptor = new BrowserFileEncryptor(content.length, (offset, length) => {
    reads += 1;
    largestRead = Math.max(largestRead, length);
    return content.slice(offset, offset + length);
  });
  assert.equal(reads, 0, "construction must not eagerly read the file");
  const records = [encryptor.nextRecord()];
  assert(reads > 0 && reads < Math.ceil(content.length / maxChunkSize));
  while (true) {
    const record = encryptor.nextRecord();
    if (record === undefined) break;
    records.push(record);
  }
  const staged = encryptor.finish("streamed.bin", "application/octet-stream");
  encryptor.free();

  assert.equal(staged.size, content.length);
  assert(largestRead <= maxChunkSize);
  assert.equal(staged.name, "streamed.bin");
  assert.equal(staged.content_type, "application/octet-stream");
  assert.equal(staged.records.length, records.length);
  assert.equal(staged.address, records.at(-1).address);
  assert.equal(staged.data_map_size, records.at(-1).content.length);
  assert.equal(verifyRecord(staged.blake3, content), staged.blake3);
  assert.deepEqual(
    staged.records,
    records.map((record) => ({
      address: record.address,
      size: record.content.length,
    })),
  );
  assert.equal(decodePublicDataMap(records.at(-1).content).length, 3);
  assert.equal(staged.chunks.length, 4);
  assert.deepEqual(
    decryptPublicFile(
      records.at(-1).content,
      records.slice(0, -1).map((record) => record.content),
    ),
    content,
  );
});
