import assert from "node:assert/strict";
import test from "node:test";
import { blake3 } from "@noble/hashes/blake3.js";
import { ml_dsa65 } from "@noble/post-quantum/ml-dsa.js";
import {
  BrowserNodeClientPool,
  bytesToHex,
  hexToBytes,
  mungeOfferIceCredentials,
  parseResponseFrame,
  parseWebRtcDirectMultiaddr,
  readResponseFrame,
  serverAnswerFromEndpoint,
  verifyChunk,
  verifyHelloIdentity,
} from "./protocol.js";

test("hex conversion enforces fixed widths", () => {
  const value = "ab".repeat(32);
  assert.equal(bytesToHex(hexToBytes(value, 32)), value);
  assert.throws(() => hexToBytes("abcd", 32), /Expected 32 bytes/);
  assert.throws(() => hexToBytes("zz", 1), /hexadecimal/);
});

test("response framing preserves a raw binary body", () => {
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

test("response framing completes without waiting for stream EOF", async () => {
  const header = new TextEncoder().encode(
    JSON.stringify({
      version: 3,
      request_id: 10,
      status: "ok",
      content_length: 3,
      type: "chunk",
    }),
  );
  const frame = new Uint8Array(4 + header.length + 3);
  new DataView(frame.buffer).setUint32(0, header.length, false);
  frame.set(header, 4);
  frame.set([4, 5, 6], 4 + header.length);
  let requestedAnotherChunk = false;
  const stream = {
    async *[Symbol.asyncIterator]() {
      yield frame.subarray(0, 2);
      yield frame.subarray(2, frame.length);
      requestedAnotherChunk = true;
      await new Promise(() => {});
    },
  };

  const received = await readResponseFrame(stream);
  assert.deepEqual(received, frame);
  assert.equal(requestedAnotherChunk, false);
});

test("WebRTC Direct multiaddresses carry one stable certificate hash", () => {
  const peerId = "ab".repeat(32);
  const multiaddr = webrtc_directMultiaddr(
    "ip4",
    "127.0.0.1",
    24000,
    peerId,
    0x11,
  );
  const parsed = parseWebRtcDirectMultiaddr(multiaddr);

  assert.equal(parsed.hostProtocol, "ip4");
  assert.equal(parsed.host, "127.0.0.1");
  assert.equal(parsed.port, 24000);
  assert.equal(parsed.peerId, peerId);
  assert.deepEqual([...parsed.certificateHash], Array(32).fill(0x11));
  assert.throws(
    () =>
      parseWebRtcDirectMultiaddr(
        `/ip4/127.0.0.1/udp/24000/webrtc-direct/p2p/${peerId}`,
      ),
    /certhash|incomplete/,
  );
  assert.throws(
    () =>
      parseWebRtcDirectMultiaddr(
        `/dns/node.example/udp/24000/webrtc-direct/certhash/${certificateMultihash(0x11)}/p2p/${peerId}`,
      ),
    /literal IP/,
  );
});

test("the browser synthesizes a certificate-pinned ICE-lite answer", () => {
  const endpoint = webrtc_directMultiaddr(
    "ip4",
    "127.0.0.1",
    24000,
    "ab".repeat(32),
    0x11,
  );
  const credential = `saorsa+webrtc+v1/${"a".repeat(32)}`;
  const answer = serverAnswerFromEndpoint(endpoint, credential);

  assert.equal(answer.type, "answer");
  assert.match(answer.sdp, /a=ice-lite/);
  assert.match(
    answer.sdp,
    /m=application 24000 UDP\/DTLS\/SCTP webrtc-datachannel/,
  );
  assert.match(
    answer.sdp,
    new RegExp(`a=ice-ufrag:${credential.replaceAll("+", "\\+")}`),
  );
  assert.match(answer.sdp, /a=fingerprint:sha-256 11:11:11:11/);
  assert.match(answer.sdp, /a=setup:passive/);
});

test("the browser offer uses the Saorsa credential as ufrag and password", () => {
  const credential = `saorsa+webrtc+v1/${"b".repeat(32)}`;
  const offer = mungeOfferIceCredentials(
    {
      type: "offer",
      sdp: "v=0\r\na=ice-ufrag:browser-generated\r\na=ice-pwd:browser-secret\r\n",
    },
    credential,
  );
  assert.match(
    offer.sdp,
    new RegExp(`a=ice-ufrag:${credential.replaceAll("+", "\\+")}`),
  );
  assert.match(
    offer.sdp,
    new RegExp(`a=ice-pwd:${credential.replaceAll("+", "\\+")}`),
  );
});

test("the browser reuses a bounded pool of WebRTC node connections", async () => {
  const endpoints = [0x11, 0x22, 0x33].map((hashByte, index) =>
    webrtc_directMultiaddr(
      "ip4",
      "127.0.0.1",
      24000 + index,
      (index + 1).toString(16).padStart(2, "0").repeat(32),
      hashByte,
    ),
  );
  const created = [];
  const closed = [];
  const pool = new BrowserNodeClientPool({
    maxClients: 2,
    clientFactory: (endpoint) => {
      const client = { endpoint, close: () => closed.push(endpoint) };
      created.push(client);
      return client;
    },
  });

  let firstClient;
  await pool.withClient(endpoints[0], async (client) => {
    firstClient = client;
  });
  await pool.withClient(endpoints[0], async (client) => {
    assert.equal(client, firstClient);
  });
  await pool.withClient(endpoints[1], async () => {});
  await pool.withClient(endpoints[2], async () => {});

  assert.equal(created.length, 3);
  assert.equal(pool.size, 2);
  assert.deepEqual(closed, [endpoints[0]]);
  pool.close();
  assert.equal(closed.length, 3);
});

test("the WebRTC client pool waits instead of exceeding its connection cap", async () => {
  const firstEndpoint = webrtc_directMultiaddr(
    "ip4",
    "127.0.0.1",
    24000,
    "11".repeat(32),
    0x11,
  );
  const secondEndpoint = webrtc_directMultiaddr(
    "ip4",
    "127.0.0.1",
    24001,
    "22".repeat(32),
    0x22,
  );
  const created = [];
  let releaseFirst;
  let secondEntered = false;
  const pool = new BrowserNodeClientPool({
    maxClients: 1,
    clientFactory: (endpoint) => {
      const client = { endpoint, close() {} };
      created.push(client);
      return client;
    },
  });
  const first = pool.withClient(
    firstEndpoint,
    () => new Promise((resolve) => (releaseFirst = resolve)),
  );
  const second = pool.withClient(secondEndpoint, async () => {
    secondEntered = true;
  });

  await Promise.resolve();
  await Promise.resolve();
  assert.equal(created.length, 1);
  assert.equal(secondEntered, false);
  releaseFirst();
  await Promise.all([first, second]);
  assert.equal(created.length, 2);
  assert.equal(secondEntered, true);
  pool.close();
});

test("HELLO binds the persistent ANT identity to the WebRTC endpoint", () => {
  const challenge = new Uint8Array(32).fill(0x22);
  const { publicKey, secretKey } = ml_dsa65.keygen(
    new Uint8Array(32).fill(0x33),
  );
  const peerId = bytesToHex(blake3(publicKey));
  const multiaddr = webrtc_directMultiaddr(
    "ip4",
    "127.0.0.1",
    24000,
    peerId,
    0x44,
  );
  const transcript = concatBytes(
    new TextEncoder().encode("autonomi-webrtc-direct-hello-v1\0"),
    challenge,
    new TextEncoder().encode(peerId),
    new TextEncoder().encode(multiaddr),
  );
  const header = {
    type: "hello",
    protocol: "autonomi.web.poc.v3",
    challenge: bytesToHex(challenge),
    peer_id: peerId,
    public_key: bytesToHex(publicKey),
    signature: bytesToHex(ml_dsa65.sign(transcript, secretKey)),
    endpoint: { multiaddr },
  };

  assert.equal(verifyHelloIdentity(header, multiaddr, challenge), peerId);
  assert.throws(
    () =>
      verifyHelloIdentity(
        { ...header, challenge: "00".repeat(32) },
        multiaddr,
        challenge,
      ),
    /different HELLO challenge/,
  );
});

test("BLAKE3 verification accepts the canonical empty hash", () => {
  const emptyHash =
    "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
  assert.equal(verifyChunk(emptyHash, new Uint8Array()), emptyHash);
  assert.throws(
    () => verifyChunk("00".repeat(32), new Uint8Array()),
    /BLAKE3 mismatch/,
  );
});

function certificateMultihash(byte) {
  const multihash = Uint8Array.from([0x12, 0x20, ...Array(32).fill(byte)]);
  return `u${Buffer.from(multihash).toString("base64url")}`;
}

function concatBytes(...parts) {
  const output = new Uint8Array(
    parts.reduce((length, part) => length + part.length, 0),
  );
  let offset = 0;
  for (const part of parts) {
    output.set(part, offset);
    offset += part.length;
  }
  return output;
}

function webrtc_directMultiaddr(hostProtocol, host, port, peerId, hashByte) {
  return `/${hostProtocol}/${host}/udp/${port}/webrtc-direct/certhash/${certificateMultihash(hashByte)}/p2p/${peerId}`;
}
