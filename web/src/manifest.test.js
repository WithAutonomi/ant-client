import assert from "node:assert/strict";
import test from "node:test";
import { parseBrowserManifest } from "./manifest.js";

test("browser manifest validates and normalizes endpoints and files", () => {
  const manifest = parseBrowserManifest({
    version: 5,
    network_id: "local-test",
    created_at: "2026-08-03T00:00:00Z",
    payment: paymentNetwork(),
    endpoints: [
      {
        multiaddr: webrtc_directMultiaddr("AA".repeat(32), 0xbb),
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

  assert.equal(
    manifest.endpoints[0].multiaddr,
    webrtc_directMultiaddr("AA".repeat(32), 0xbb),
  );
  assert.equal(manifest.files[0].address, "cc".repeat(32));
  assert.equal(manifest.files[0].blake3, "dd".repeat(32));
  assert.deepEqual(
    manifest.files[0].chunks.map((chunk) => chunk.index),
    [0, 1, 2],
  );
  assert.equal(manifest.files[0].replicas, 5);
  assert.deepEqual(manifest.payment, paymentNetwork());
});

test("browser manifest rejects missing endpoints and malformed multiaddresses", () => {
  assert.throws(
    () =>
      parseBrowserManifest({
        version: 5,
        network_id: "test",
        payment: paymentNetwork(),
        endpoints: [],
      }),
    /no WebRtcDirect endpoints/,
  );
  assert.throws(
    () =>
      parseBrowserManifest({
        version: 5,
        network_id: "test",
        payment: paymentNetwork(),
        endpoints: [
          {
            multiaddr:
              "/ip4/127.0.0.1/udp/22000/webrtc-direct/certhash/uAA/p2p/wrong",
          },
        ],
      }),
    /multihash|hexadecimal|Expected 32 bytes/,
  );
});

test("browser manifest requires public payment contract configuration", () => {
  const endpoint = { multiaddr: webrtc_directMultiaddr("aa".repeat(32), 0xbb) };
  assert.throws(
    () => parseBrowserManifest({ version: 5, network_id: "test", endpoints: [endpoint] }),
    /missing field.*payment|payment network/i,
  );
  assert.throws(
    () =>
      parseBrowserManifest({
        version: 5,
        network_id: "test",
        endpoints: [endpoint],
        payment: { ...paymentNetwork(), rpc_url: "file:///tmp/anvil" },
      }),
    /HTTP or HTTPS/,
  );
});

function paymentNetwork() {
  return {
    rpc_url: "http://127.0.0.1:8545/",
    payment_token_address: `0x${"11".repeat(20)}`,
    payment_vault_address: `0x${"22".repeat(20)}`,
  };
}

function webrtc_directMultiaddr(peerId, certificateByte) {
  const multihash = Uint8Array.from([
    0x12,
    0x20,
    ...Array(32).fill(certificateByte),
  ]);
  const certhash = `u${Buffer.from(multihash).toString("base64url")}`;
  return `/ip4/127.0.0.1/udp/22000/webrtc-direct/certhash/${certhash}/p2p/${peerId}`;
}
