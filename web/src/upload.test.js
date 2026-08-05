import assert from "node:assert/strict";
import test from "node:test";
import { encryptPublicFile } from "./upload.js";

test("adapts native ant-core WASM output to browser file metadata", async () => {
  const content = new TextEncoder().encode("browser fixture");
  const dataMap = Uint8Array.of(7, 8, 9);
  const encryptedRecord = Uint8Array.of(1, 2, 3);
  const encrypted = await encryptPublicFile(content, {
    name: "fixture.txt",
    contentType: "text/plain",
    encrypt: () => ({
      address: "44".repeat(32),
      blake3: "55".repeat(32),
      data_map_size: dataMap.length,
      chunks: [
        {
          index: 0,
          dst_hash: "66".repeat(32),
          src_hash: "77".repeat(32),
          src_size: content.length,
        },
      ],
      records: [
        { address: "66".repeat(32), content: encryptedRecord },
        { address: "44".repeat(32), content: dataMap },
      ],
    }),
  });

  assert.equal(encrypted.descriptor.name, "fixture.txt");
  assert.equal(encrypted.descriptor.address, "44".repeat(32));
  assert.equal(encrypted.descriptor.content_type, "text/plain");
  assert.equal(encrypted.descriptor.size, content.length);
  assert.equal(encrypted.records.length, 2);
  assert.deepEqual(encrypted.records[0].content, encryptedRecord);
  assert.deepEqual(encrypted.records[1].content, dataMap);
});
