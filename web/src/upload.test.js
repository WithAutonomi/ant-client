import assert from "node:assert/strict";
import test from "node:test";
import { brotliCompressSync, brotliDecompressSync, constants } from "node:zlib";
import { blake3 } from "@noble/hashes/blake3.js";
import { decryptSelfEncryptedChunk } from "./file.js";
import { bytesToHex } from "./protocol.js";
import {
  decodePublicDataMap,
  encodePublicDataMap,
  encryptPublicFile,
} from "./upload.js";

const compress = (input) =>
  new Uint8Array(
    brotliCompressSync(input, {
      params: { [constants.BROTLI_PARAM_QUALITY]: 6 },
    }),
  );
const decompress = (input) => new Uint8Array(brotliDecompressSync(input));

test("browser encryption reproduces the native self_encryption 0.36 vector", async () => {
  const content = new TextEncoder().encode("browser whole-file fixture\n".repeat(160));
  const encrypted = await encryptPublicFile(content, {
    name: "fixture.txt",
    contentType: "text/plain",
    compress,
  });

  assert.deepEqual(
    encrypted.descriptor.chunks.map((chunk) => chunk.dst_hash),
    [
      "c024c6884a2f39be7ba07c3d9636efedeb94df7397fcd38bac5ae904643c5cc9",
      "350a88e6eb0b2a3e774107a212a272b4191af69ca4366a4b91f5a1e5872c459a",
      "d73db5a8b0be3b571b40d2b80ff490fe45e135f1992c5863ecb78e25d00ceddb",
    ],
  );
  assert.equal(
    encrypted.descriptor.address,
    "0d3636dd504d04a236f7e104909234766f077fa7e1ca4a18293d3d168d5f169b",
  );
  assert.deepEqual(
    decodePublicDataMap(encrypted.records.at(-1).content),
    encrypted.descriptor.chunks,
  );

  const sourceHashes = encrypted.descriptor.chunks.map((chunk) => chunk.src_hash);
  const plaintext = await Promise.all(
    encrypted.descriptor.chunks.map((chunk, index) =>
      decryptSelfEncryptedChunk(
        chunk,
        encrypted.records[index].content,
        sourceHashes.map((hash) => Uint8Array.from(Buffer.from(hash, "hex"))),
        0,
        decompress,
      ),
    ),
  );
  const reconstructed = new Uint8Array(
    plaintext.reduce((total, chunk) => total + chunk.length, 0),
  );
  let offset = 0;
  for (const chunk of plaintext) {
    reconstructed.set(chunk, offset);
    offset += chunk.length;
  }
  assert.deepEqual(reconstructed, content);
  assert.equal(bytesToHex(blake3(reconstructed)), encrypted.descriptor.blake3);
});

test("public DataMap encoder matches rmp-serde's compact native representation", () => {
  const chunks = [0, 1, 2].map((index) => ({
    index,
    dst_hash: (11 + index).toString(16).padStart(2, "0").repeat(32),
    src_hash: (21 + index).toString(16).padStart(2, "0").repeat(32),
    src_size: 100 + index,
  }));
  const expected =
    "9301939400dc00200b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0bdc00201515151515151515151515151515151515151515151515151515151515151515649401dc00200c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0cdc00201616161616161616161616161616161616161616161616161616161616161616659402dc00200d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0ddc0020171717171717171717171717171717171717171717171717171717171717171766c0";
  assert.equal(bytesToHex(encodePublicDataMap(chunks)), expected);
  assert.deepEqual(decodePublicDataMap(Uint8Array.from(Buffer.from(expected, "hex"))), chunks);
});
