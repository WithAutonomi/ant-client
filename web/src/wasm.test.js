import assert from "node:assert/strict";
import test from "node:test";
import {
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
