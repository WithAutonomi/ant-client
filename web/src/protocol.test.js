import assert from "node:assert/strict";
import test from "node:test";
import {
  BrowserNetworkClient,
  BrowserNodeClient,
  mungeOfferIceCredentials,
  parseResponseFrame,
  parseWebRtcDirectMultiaddr,
  paymentQuoteHash,
  serverAnswerFromEndpoint,
} from "../pkg/ant_core.js";

test("Rust/WASM parses stable certificate-pinned WebRTC Direct addresses", () => {
  const peerId = "ab".repeat(32);
  const multiaddr = webRtcDirectMultiaddr(peerId, 0x11);
  const parsed = parseWebRtcDirectMultiaddr(multiaddr);

  assert.equal(parsed.hostProtocol, "ip4");
  assert.equal(parsed.host, "127.0.0.1");
  assert.equal(parsed.port, 24000);
  assert.equal(parsed.peerId, peerId);
  assert.deepEqual([...parsed.certificateHash], Array(32).fill(0x11));
  assert.throws(
    () =>
      parseWebRtcDirectMultiaddr(
        `/dns/node.example/udp/24000/webrtc-direct/certhash/${certificateMultihash(0x11)}/p2p/${peerId}`,
      ),
    /literal IP/,
  );

  const node = new BrowserNodeClient(multiaddr);
  node.close();
  const network = new BrowserNetworkClient([{ multiaddr }]);
  network.close();
});

test("Rust/WASM validates response framing with a raw binary body", () => {
  const header = new TextEncoder().encode(
    JSON.stringify({
      version: 3,
      request_id: 9,
      status: "ok",
      content_length: 3,
      type: "chunk",
      address: "11".repeat(32),
      size: 3,
    }),
  );
  const frame = new Uint8Array(4 + header.length + 3);
  new DataView(frame.buffer).setUint32(0, header.length, false);
  frame.set(header, 4);
  frame.set([1, 2, 3], 4 + header.length);

  const parsed = parseResponseFrame(frame);
  assert.equal(parsed.header.request_id, 9);
  assert.deepEqual([...parsed.content], [1, 2, 3]);
});

test("Rust/WASM synthesizes the pinned answer and shared ICE credentials", () => {
  const endpoint = webRtcDirectMultiaddr("ab".repeat(32), 0x11);
  const credential = `saorsa+webrtc+v1/${"a".repeat(32)}`;
  const answer = serverAnswerFromEndpoint(endpoint, credential);

  assert.equal(answer.type, "answer");
  assert.match(answer.sdp, /a=ice-lite/);
  assert.match(
    answer.sdp,
    /m=application 24000 UDP\/DTLS\/SCTP webrtc-datachannel/,
  );
  assert.match(answer.sdp, /a=fingerprint:sha-256 11:11:11:11/);

  const offer = mungeOfferIceCredentials(
    {
      type: "offer",
      sdp: "v=0\r\na=ice-ufrag:old\r\na=ice-pwd:secret\r\n",
    },
    credential,
  );
  assert.match(offer.sdp, new RegExp(`a=ice-ufrag:${escapeRegex(credential)}`));
  assert.match(offer.sdp, new RegExp(`a=ice-pwd:${escapeRegex(credential)}`));
});

test("Rust/WASM uses the native EVM PaymentQuote Keccak hash", () => {
  assert.equal(
    paymentQuoteHash(Uint8Array.of(0, 1), Uint8Array.of(2), Uint8Array.of(3)),
    "d98f2e8134922f73748703c8e7084d42f13d2fa1439936ef5a3abcf5646fe83f",
  );
});

function certificateMultihash(byte) {
  const multihash = Uint8Array.from([0x12, 0x20, ...Array(32).fill(byte)]);
  return `u${Buffer.from(multihash).toString("base64url")}`;
}

function webRtcDirectMultiaddr(peerId, certificateByte) {
  return `/ip4/127.0.0.1/udp/24000/webrtc-direct/certhash/${certificateMultihash(certificateByte)}/p2p/${peerId}`;
}

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
