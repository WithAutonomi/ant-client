import assert from "node:assert/strict";
import test from "node:test";
import { blake3 } from "@noble/hashes/blake3.js";
import { downloadPublicFile } from "./file.js";
import { bytesToHex } from "./protocol.js";

test("downloads records and delegates reconstruction to ant-core WASM", async () => {
  const content = new TextEncoder().encode("browser whole-file fixture\n".repeat(160));
  const dataMapContent = Uint8Array.of(9, 8, 7);
  const dataMapAddress = bytesToHex(blake3(dataMapContent));
  const chunks = [0, 1, 2].map((index) => ({
    index,
    dst_hash: (index + 1).toString(16).padStart(2, "0").repeat(32),
    src_hash: (index + 4).toString(16).padStart(2, "0").repeat(32),
    src_size: content.length / 3,
  }));
  const encryptedByAddress = new Map(
    chunks.map((chunk, index) => [chunk.dst_hash, Uint8Array.of(index)]),
  );
  encryptedByAddress.set(dataMapAddress, dataMapContent);
  const requested = [];
  const downloadChunk = async (_seeds, address) => {
    requested.push(address);
    const bytes = encryptedByAddress.get(address);
    if (!bytes) throw new Error(`No fixture record ${address}`);
    return { content: bytes, node: { peer_id: "11".repeat(32) } };
  };

  const result = await downloadPublicFile(
    [],
    {
      name: "fixture.txt",
      address: dataMapAddress,
      size: content.length,
      content_type: "text/plain",
      blake3: bytesToHex(blake3(content)),
      data_map_size: dataMapContent.length,
      chunks,
    },
    {
      downloadChunk,
      decodeDataMap: (receivedDataMap) => {
        assert.deepEqual(receivedDataMap, dataMapContent);
        return chunks;
      },
      decrypt: (receivedDataMap, encryptedContents) => {
        assert.deepEqual(receivedDataMap, dataMapContent);
        assert.deepEqual(
          encryptedContents,
          chunks.map((_, index) => Uint8Array.of(index)),
        );
        return content;
      },
    },
  );

  assert.deepEqual(result.content, content);
  assert.equal(result.hash, bytesToHex(blake3(content)));
  assert.deepEqual(new Set(requested), new Set([dataMapAddress, ...chunks.map((c) => c.dst_hash)]));
});
