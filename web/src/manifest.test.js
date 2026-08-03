import assert from "node:assert/strict";
import test from "node:test";
import { parseBrowserManifest } from "./manifest.js";

test("browser manifest validates and normalizes endpoints and files", () => {
  const manifest = parseBrowserManifest({
    version: 2,
    network_id: "local-test",
    created_at: "2026-08-03T00:00:00Z",
    endpoints: [
      {
        peer_id: "AA".repeat(32),
        url: "https://127.0.0.1:22000/autonomi/webtransport/v1",
        certificate_sha256: "BB".repeat(32),
      },
    ],
    files: [
      {
        name: "hello.txt",
        address: "CC".repeat(32),
        size: 12,
        content_type: "text/plain",
        blake3: "DD".repeat(32),
        data_map_size: 128,
        chunks: [
          { index: 2, dst_hash: "13".repeat(32), src_hash: "23".repeat(32), src_size: 4 },
          { index: 0, dst_hash: "11".repeat(32), src_hash: "21".repeat(32), src_size: 4 },
          { index: 1, dst_hash: "12".repeat(32), src_hash: "22".repeat(32), src_size: 4 },
        ],
        replicas: 5,
      },
    ],
  });

  assert.equal(manifest.endpoints[0].peer_id, "aa".repeat(32));
  assert.equal(manifest.files[0].address, "cc".repeat(32));
  assert.equal(manifest.files[0].blake3, "dd".repeat(32));
  assert.deepEqual(
    manifest.files[0].chunks.map((chunk) => chunk.index),
    [0, 1, 2],
  );
  assert.equal(manifest.files[0].replicas, 5);
});

test("browser manifest rejects missing endpoints and malformed hashes", () => {
  assert.throws(
    () => parseBrowserManifest({ version: 2, network_id: "test", endpoints: [] }),
    /no WebTransport endpoints/,
  );
  assert.throws(
    () =>
      parseBrowserManifest({
        version: 2,
        network_id: "test",
        endpoints: [
          {
            peer_id: "wrong",
            url: "https://127.0.0.1:22000/path",
            certificate_sha256: "bb".repeat(32),
          },
        ],
      }),
    /hexadecimal|Expected 32 bytes/,
  );
});
