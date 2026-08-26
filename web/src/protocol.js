import { bytesToHex as nobleBytesToHex } from "@noble/hashes/utils.js";
import { blake3 } from "@noble/hashes/blake3.js";
import { ml_dsa65 } from "@noble/post-quantum/ml-dsa.js";
import {
  BrowserIterativeLookup as BrowserIterativeLookupNative,
  verifyRecord as verifyRecordNative,
} from "../pkg/ant_core.js";

export const PROTOCOL_VERSION = 3;
export const PROTOCOL_NAME = "autonomi.web.poc.v3";
export const WEBRTC_DIRECT_DATA_CHANNEL = "autonomi.web.v3";
export const MAX_CHUNK_SIZE = 4 * 1024 * 1024;
export const MAX_RESPONSE_HEADER_BYTES = 64 * 1024;
const MAX_RESPONSE_BYTES = 4 + MAX_RESPONSE_HEADER_BYTES + MAX_CHUNK_SIZE;
const MAX_WEBRTC_DIRECT_MULTIADDR_LENGTH = 2048;
const WEBRTC_WRITE_CHUNK_BYTES = 16 * 1024;
const MAX_BUFFERED_AMOUNT = 2 * 1024 * 1024;
const REQUEST_TIMEOUT_MS = 10_000;
const ICE_CREDENTIAL_PREFIX = "saorsa+webrtc+v1/";
const ICE_RANDOM_LENGTH = 32;
const ICE_ALPHABET =
  "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const SHA2_256_MULTIHASH_CODE = 0x12;
const SHA2_256_MULTIHASH_LENGTH = 32;
const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });
const HELLO_DOMAIN = encoder.encode("autonomi-webrtc-direct-hello-v1\0");

let nextRequestId = 1;

export function hexToBytes(value, expectedLength) {
  const normalized = value.trim().replace(/^0x/i, "").replaceAll(":", "");
  if (!/^[0-9a-f]*$/i.test(normalized) || normalized.length % 2 !== 0) {
    throw new Error("Expected an even-length hexadecimal value");
  }
  const bytes = Uint8Array.from(
    normalized.match(/.{2}/g)?.map((pair) => Number.parseInt(pair, 16)) ?? [],
  );
  if (expectedLength !== undefined && bytes.length !== expectedLength) {
    throw new Error(
      `Expected ${expectedLength} bytes, received ${bytes.length}`,
    );
  }
  return bytes;
}

export function bytesToHex(bytes) {
  return nobleBytesToHex(bytes);
}

function concatBytes(...parts) {
  const result = new Uint8Array(
    parts.reduce((total, part) => total + part.length, 0),
  );
  let offset = 0;
  for (const part of parts) {
    result.set(part, offset);
    offset += part.length;
  }
  return result;
}

export function verifyChunk(address, content) {
  const expected = address.trim().replace(/^0x/i, "").toLowerCase();
  hexToBytes(expected, 32);
  return verifyRecordNative(expected, content);
}

export function parseResponseFrame(frame) {
  const { header, contentOffset, frameLength } = parseResponseHeader(frame);
  if (frame.length !== frameLength) {
    throw new Error(
      `Response length mismatch: declared ${header.content_length} content bytes`,
    );
  }
  return { header, content: frame.slice(contentOffset) };
}

function parseResponseHeader(frame) {
  if (!(frame instanceof Uint8Array) || frame.length < 4) {
    throw new Error("Response ended before its four-byte header length");
  }
  const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength);
  const headerLength = view.getUint32(0, false);
  if (headerLength === 0 || headerLength > MAX_RESPONSE_HEADER_BYTES) {
    throw new Error(`Invalid response header length ${headerLength}`);
  }
  const contentOffset = 4 + headerLength;
  if (contentOffset > frame.length) {
    throw new Error("Response ended inside its JSON header");
  }

  let header;
  try {
    header = JSON.parse(decoder.decode(frame.subarray(4, contentOffset)));
  } catch (error) {
    throw new Error(`Invalid response JSON: ${error.message}`, {
      cause: error,
    });
  }
  if (header.version !== PROTOCOL_VERSION) {
    throw new Error(`Unsupported response version ${header.version}`);
  }
  if (
    !Number.isSafeInteger(header.content_length) ||
    header.content_length < 0 ||
    header.content_length > MAX_CHUNK_SIZE
  ) {
    throw new Error(`Invalid response content length ${header.content_length}`);
  }
  return {
    header,
    contentOffset,
    frameLength: contentOffset + header.content_length,
  };
}

export async function readResponseFrame(stream, limit = MAX_RESPONSE_BYTES) {
  if (!Number.isSafeInteger(limit) || limit < 4) {
    throw new Error(`Invalid response limit ${limit}`);
  }
  let frame = new Uint8Array(Math.min(limit, 8 * 1024));
  let total = 0;
  let expectedLength;
  for await (const value of stream) {
    const chunk = value instanceof Uint8Array ? value : value.subarray();
    const nextTotal = total + chunk.length;
    if (nextTotal > limit) {
      throw new Error(`Response exceeded the ${limit}-byte client limit`);
    }
    if (nextTotal > frame.length) {
      let capacity = frame.length;
      while (capacity < nextTotal) capacity = Math.min(limit, capacity * 2);
      const grown = new Uint8Array(capacity);
      grown.set(frame.subarray(0, total));
      frame = grown;
    }
    frame.set(chunk, total);
    total = nextTotal;

    if (expectedLength === undefined && total >= 4) {
      const headerLength = new DataView(
        frame.buffer,
        frame.byteOffset,
        total,
      ).getUint32(0, false);
      if (headerLength === 0 || headerLength > MAX_RESPONSE_HEADER_BYTES) {
        throw new Error(`Invalid response header length ${headerLength}`);
      }
      if (total >= 4 + headerLength) {
        expectedLength = parseResponseHeader(
          frame.subarray(0, total),
        ).frameLength;
        if (expectedLength > limit) {
          throw new Error(`Response exceeded the ${limit}-byte client limit`);
        }
      }
    }

    if (expectedLength !== undefined && total >= expectedLength) {
      if (total !== expectedLength) {
        throw new Error("Response contains bytes after its declared frame");
      }
      return frame.slice(0, total);
    }
  }
  throw new Error("Response ended before its declared frame was complete");
}

function decodeBase64Url(value) {
  if (!/^[A-Za-z0-9_-]+$/.test(value)) {
    throw new Error("Certificate multihash is not valid unpadded base64url");
  }
  const padding = "=".repeat((4 - (value.length % 4)) % 4);
  let binary;
  try {
    binary = atob(value.replaceAll("-", "+").replaceAll("_", "/") + padding);
  } catch (error) {
    throw new Error("Certificate multihash is not valid unpadded base64url", {
      cause: error,
    });
  }
  return Uint8Array.from(binary, (character) => character.charCodeAt(0));
}

function decodeCertificateMultihash(value) {
  if (typeof value !== "string" || !value.startsWith("u")) {
    throw new Error("Certificate multihash must use base64url multibase (`u`)");
  }
  const decoded = decodeBase64Url(value.slice(1));
  if (
    decoded.length !== 34 ||
    decoded[0] !== SHA2_256_MULTIHASH_CODE ||
    decoded[1] !== SHA2_256_MULTIHASH_LENGTH
  ) {
    throw new Error(
      "Certificate multihash must contain a 32-byte SHA-256 digest",
    );
  }
  return decoded.slice(2);
}

function validateIpv4(value) {
  const octets = value.split(".");
  if (
    octets.length !== 4 ||
    octets.some(
      (octet) =>
        !/^(0|[1-9][0-9]{0,2})$/.test(octet) ||
        Number.parseInt(octet, 10) > 255,
    )
  ) {
    throw new Error(`Invalid IPv4 address ${value}`);
  }
  return value;
}

function endpointMultiaddr(endpoint) {
  if (typeof endpoint === "string") return endpoint;
  if (endpoint && typeof endpoint.multiaddr === "string")
    return endpoint.multiaddr;
  throw new Error("A WebRtcDirect multiaddress is required");
}

export function parseWebRtcDirectMultiaddr(endpoint) {
  const multiaddr = endpointMultiaddr(endpoint).trim();
  if (
    multiaddr.length === 0 ||
    multiaddr.length > MAX_WEBRTC_DIRECT_MULTIADDR_LENGTH ||
    !multiaddr.startsWith("/")
  ) {
    throw new Error("Invalid WebRtcDirect multiaddress length or prefix");
  }
  const segments = multiaddr.split("/");
  if (segments.length !== 10) {
    throw new Error("WebRtcDirect multiaddress is incomplete");
  }

  const hostProtocol = segments[1];
  const hostValue = segments[2];
  if (!hostValue) throw new Error("WebRtcDirect multiaddress host is empty");
  if (hostProtocol === "ip4") {
    validateIpv4(hostValue);
  } else if (hostProtocol === "ip6") {
    if (!hostValue.includes(":"))
      throw new Error(`Invalid IPv6 address ${hostValue}`);
  } else {
    throw new Error(
      "WebRTC Direct multiaddresses must use a literal IP address",
    );
  }
  if (segments[3] !== "udp") {
    throw new Error("WebRtcDirect multiaddress must use UDP");
  }
  if (!/^[0-9]{1,5}$/.test(segments[4])) {
    throw new Error("WebRtcDirect multiaddress has an invalid UDP port");
  }
  const port = Number.parseInt(segments[4], 10);
  if (port < 1 || port > 65535) {
    throw new Error("WebRtcDirect multiaddress has an invalid UDP port");
  }
  if (segments[5] !== "webrtc-direct") {
    throw new Error("WebRTC Direct multiaddress must contain /webrtc-direct");
  }
  if (segments[6] !== "certhash" || !segments[7]) {
    throw new Error(
      "WebRTC Direct multiaddress must contain exactly one certhash",
    );
  }
  const certificateHash = decodeCertificateMultihash(segments[7]);
  if (segments[8] !== "p2p") {
    throw new Error("WebRtcDirect multiaddress must end with /p2p/<peer-id>");
  }
  const peerId = segments[9]?.toLowerCase() ?? "";
  hexToBytes(peerId, 32);
  return {
    multiaddr,
    hostProtocol,
    host: hostValue,
    port,
    peerId,
    certificateHash,
  };
}

const normalizeEndpoint = parseWebRtcDirectMultiaddr;

export function verifyHelloIdentity(header, expectedEndpoint, challengeBytes) {
  const endpoint = normalizeEndpoint(expectedEndpoint);
  if (!(challengeBytes instanceof Uint8Array) || challengeBytes.length !== 32) {
    throw new Error("HELLO verification requires a 32-byte challenge");
  }
  if (header.type !== "hello") throw new Error("Expected a HELLO response");
  if (header.protocol !== PROTOCOL_NAME) {
    throw new Error(`Unsupported browser protocol ${header.protocol}`);
  }
  hexToBytes(header.peer_id, 32);
  if (header.challenge?.toLowerCase() !== bytesToHex(challengeBytes)) {
    throw new Error("Node signed a different HELLO challenge");
  }
  const advertisedEndpoint = normalizeEndpoint(header.endpoint);
  if (
    advertisedEndpoint.multiaddr !== endpoint.multiaddr ||
    advertisedEndpoint.peerId !== header.peer_id.toLowerCase()
  ) {
    throw new Error("Node advertised a different WebRTC Direct endpoint");
  }
  if (header.peer_id.toLowerCase() !== endpoint.peerId.toLowerCase()) {
    throw new Error(
      `Endpoint identity mismatch: expected ${endpoint.peerId}, received ${header.peer_id}`,
    );
  }
  const publicKey = hexToBytes(header.public_key);
  const signature = hexToBytes(header.signature);
  if (publicKey.length !== ml_dsa65.lengths.publicKey) {
    throw new Error(`HELLO has a ${publicKey.length}-byte public key`);
  }
  if (signature.length !== ml_dsa65.lengths.signature) {
    throw new Error(`HELLO has a ${signature.length}-byte signature`);
  }
  if (bytesToHex(blake3(publicKey)) !== header.peer_id.toLowerCase()) {
    throw new Error("HELLO public key is not bound to the ANT peer ID");
  }
  const transcript = concatBytes(
    HELLO_DOMAIN,
    challengeBytes,
    encoder.encode(header.peer_id.toLowerCase()),
    encoder.encode(advertisedEndpoint.multiaddr),
  );
  if (!ml_dsa65.verify(signature, transcript, publicKey)) {
    throw new Error("HELLO has an invalid ML-DSA-65 signature");
  }
  return header.peer_id.toLowerCase();
}

function randomIceCredential() {
  const random = crypto.getRandomValues(new Uint8Array(ICE_RANDOM_LENGTH));
  let suffix = "";
  for (const byte of random) suffix += ICE_ALPHABET[byte % ICE_ALPHABET.length];
  return ICE_CREDENTIAL_PREFIX + suffix;
}

function certificateFingerprint(certificateHash) {
  return Array.from(certificateHash, (byte) =>
    byte.toString(16).padStart(2, "0").toUpperCase(),
  ).join(":");
}

export function serverAnswerFromEndpoint(endpoint, iceCredential) {
  const normalized = normalizeEndpoint(endpoint);
  if (
    typeof iceCredential !== "string" ||
    !iceCredential.startsWith(ICE_CREDENTIAL_PREFIX) ||
    !/^[a-zA-Z0-9+/]{22,256}$/.test(iceCredential)
  ) {
    throw new Error("Invalid Saorsa WebRTC Direct ICE credential");
  }
  const ipVersion = normalized.hostProtocol === "ip4" ? "IP4" : "IP6";
  return {
    type: "answer",
    sdp: `v=0\r
o=- 0 0 IN ${ipVersion} ${normalized.host}\r
s=-\r
t=0 0\r
a=ice-lite\r
m=application ${normalized.port} UDP/DTLS/SCTP webrtc-datachannel\r
c=IN ${ipVersion} ${normalized.host}\r
a=mid:0\r
a=ice-options:ice2\r
a=ice-ufrag:${iceCredential}\r
a=ice-pwd:${iceCredential}\r
a=fingerprint:sha-256 ${certificateFingerprint(normalized.certificateHash)}\r
a=setup:passive\r
a=sctp-port:5000\r
a=max-message-size:${WEBRTC_WRITE_CHUNK_BYTES}\r
a=candidate:1467250027 1 UDP 1467250027 ${normalized.host} ${normalized.port} typ host\r
a=end-of-candidates\r
`,
  };
}

export function mungeOfferIceCredentials(offer, iceCredential) {
  if (!offer?.sdp) throw new Error("Browser created an empty WebRTC offer");
  const sdp = offer.sdp
    .replace(/a=ice-ufrag:[^\r\n]+/, `a=ice-ufrag:${iceCredential}`)
    .replace(/a=ice-pwd:[^\r\n]+/, `a=ice-pwd:${iceCredential}`);
  if (
    !sdp.includes(`a=ice-ufrag:${iceCredential}`) ||
    !sdp.includes(`a=ice-pwd:${iceCredential}`)
  ) {
    throw new Error("Browser offer did not contain ICE credentials");
  }
  return { type: "offer", sdp };
}

class DataChannelInbox {
  constructor(channel) {
    this.queue = [];
    this.waiters = [];
    this.closed = false;
    this.error = undefined;
    channel.binaryType = "arraybuffer";
    channel.addEventListener("message", ({ data }) => {
      try {
        let message;
        if (data instanceof ArrayBuffer) {
          message = new Uint8Array(data);
        } else if (ArrayBuffer.isView(data)) {
          message = new Uint8Array(
            data.buffer,
            data.byteOffset,
            data.byteLength,
          );
        } else {
          throw new Error("Node sent a non-binary DataChannel message");
        }
        this.push(message);
      } catch (error) {
        this.fail(error);
      }
    });
    channel.addEventListener("error", (event) => {
      this.fail(event.error ?? new Error("WebRTC DataChannel failed"));
    });
    channel.addEventListener("close", () => this.finish());
  }

  push(message) {
    const waiter = this.waiters.shift();
    if (waiter) waiter.resolve({ value: message, done: false });
    else this.queue.push(message);
  }

  finish() {
    this.closed = true;
    for (const waiter of this.waiters.splice(0)) waiter.resolve({ done: true });
  }

  fail(error) {
    this.error = error;
    for (const waiter of this.waiters.splice(0)) waiter.reject(error);
  }

  next() {
    if (this.queue.length > 0) {
      return Promise.resolve({ value: this.queue.shift(), done: false });
    }
    if (this.error) return Promise.reject(this.error);
    if (this.closed) return Promise.resolve({ done: true });
    return new Promise((resolve, reject) =>
      this.waiters.push({ resolve, reject }),
    );
  }

  [Symbol.asyncIterator]() {
    return this;
  }
}

function waitForDataChannelOpen(channel, timeoutMs = REQUEST_TIMEOUT_MS) {
  if (channel.readyState === "open") return Promise.resolve();
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      cleanup();
      reject(new Error("WebRTC DataChannel opening timed out"));
    }, timeoutMs);
    const cleanup = () => {
      clearTimeout(timeout);
      channel.removeEventListener("open", opened);
      channel.removeEventListener("close", closed);
      channel.removeEventListener("error", failed);
    };
    const opened = () => {
      cleanup();
      resolve();
    };
    const closed = () => {
      cleanup();
      reject(new Error("WebRTC DataChannel closed before opening"));
    };
    const failed = (event) => {
      cleanup();
      reject(
        event.error ?? new Error("WebRTC DataChannel failed while opening"),
      );
    };
    channel.addEventListener("open", opened, { once: true });
    channel.addEventListener("close", closed, { once: true });
    channel.addEventListener("error", failed, { once: true });
  });
}

async function waitForDataChannelCapacity(channel) {
  if (channel.bufferedAmount <= MAX_BUFFERED_AMOUNT) return;
  channel.bufferedAmountLowThreshold = MAX_BUFFERED_AMOUNT / 2;
  await new Promise((resolve, reject) => {
    const drained = () => {
      cleanup();
      resolve();
    };
    const closed = () => {
      cleanup();
      reject(new Error("WebRTC DataChannel closed while draining"));
    };
    const cleanup = () => {
      channel.removeEventListener("bufferedamountlow", drained);
      channel.removeEventListener("close", closed);
    };
    channel.addEventListener("bufferedamountlow", drained, { once: true });
    channel.addEventListener("close", closed, { once: true });
  });
}

export class BrowserNodeClient {
  constructor(endpoint) {
    this.endpoint = normalizeEndpoint(endpoint);
    this.peerConnection = undefined;
    this.dataChannel = undefined;
    this.inbox = undefined;
    this.connectPromise = undefined;
    this.requestTail = Promise.resolve();
    this.peerId = undefined;
    this.helloResponse = undefined;
    this.helloPromise = undefined;
  }

  async connect() {
    if (this.dataChannel?.readyState === "open") return;
    if (this.connectPromise) return this.connectPromise;
    if (typeof RTCPeerConnection === "undefined") {
      throw new Error("This browser does not expose the RTCPeerConnection API");
    }
    this.connectPromise = this.openDirectConnection();
    try {
      await this.connectPromise;
    } catch (error) {
      this.close();
      throw error;
    } finally {
      this.connectPromise = undefined;
    }
  }

  async request(type, fields = {}, content = new Uint8Array()) {
    const operation = this.requestTail.then(() =>
      this.requestDirect(type, fields, content),
    );
    this.requestTail = operation.then(
      () => undefined,
      () => undefined,
    );
    return operation;
  }

  async openDirectConnection() {
    const configuration = { iceServers: [] };
    if (typeof RTCPeerConnection.generateCertificate === "function") {
      configuration.certificates = [
        await RTCPeerConnection.generateCertificate({
          name: "ECDSA",
          namedCurve: "P-256",
        }),
      ];
    }
    const peerConnection = new RTCPeerConnection(configuration);
    const dataChannel = peerConnection.createDataChannel(
      WEBRTC_DIRECT_DATA_CHANNEL,
      {
        ordered: true,
      },
    );
    const inbox = new DataChannelInbox(dataChannel);
    this.peerConnection = peerConnection;
    this.dataChannel = dataChannel;
    this.inbox = inbox;
    const iceCredential = randomIceCredential();
    const offer = mungeOfferIceCredentials(
      await peerConnection.createOffer(),
      iceCredential,
    );
    await peerConnection.setLocalDescription(offer);
    await peerConnection.setRemoteDescription(
      serverAnswerFromEndpoint(this.endpoint, iceCredential),
    );
    await waitForDataChannelOpen(dataChannel);
  }

  async requestDirect(type, fields, content) {
    await this.connect();
    if (!(content instanceof Uint8Array) || content.length > MAX_CHUNK_SIZE) {
      throw new Error(
        `Request content must be at most ${MAX_CHUNK_SIZE} bytes`,
      );
    }
    const requestId = nextRequestId;
    nextRequestId += 1;
    const header = encoder.encode(
      JSON.stringify({
        version: PROTOCOL_VERSION,
        request_id: requestId,
        content_length: content.length,
        type,
        ...fields,
      }),
    );
    if (header.length === 0 || header.length > MAX_RESPONSE_HEADER_BYTES) {
      throw new Error(`Request header is ${header.length} bytes`);
    }
    const frame = new Uint8Array(4 + header.length + content.length);
    new DataView(frame.buffer).setUint32(0, header.length, false);
    frame.set(header, 4);
    frame.set(content, 4 + header.length);
    for (
      let offset = 0;
      offset < frame.length;
      offset += WEBRTC_WRITE_CHUNK_BYTES
    ) {
      await waitForDataChannelCapacity(this.dataChannel);
      this.dataChannel.send(
        frame.subarray(offset, offset + WEBRTC_WRITE_CHUNK_BYTES),
      );
    }
    let timeout;
    let responseFrame;
    try {
      responseFrame = await Promise.race([
        readResponseFrame(this.inbox),
        new Promise((_, reject) => {
          timeout = setTimeout(
            () => reject(new Error("WebRTC request timed out")),
            REQUEST_TIMEOUT_MS,
          );
        }),
      ]);
    } catch (error) {
      this.close();
      throw error;
    } finally {
      clearTimeout(timeout);
    }

    const response = parseResponseFrame(responseFrame);
    if (response.header.request_id !== requestId) {
      throw new Error(
        `Response ID ${response.header.request_id} does not match request ${requestId}`,
      );
    }
    if (response.header.status === "error") {
      const error = new Error(
        response.header.message ?? "Node returned an error",
      );
      error.code = response.header.code;
      throw error;
    }
    return response;
  }

  async hello() {
    if (this.helloResponse && this.dataChannel?.readyState === "open") {
      return this.helloResponse;
    }
    if (this.helloPromise) return this.helloPromise;
    this.helloPromise = (async () => {
      const challengeBytes = crypto.getRandomValues(new Uint8Array(32));
      const challenge = bytesToHex(challengeBytes);
      const { header } = await this.request("hello", { challenge });
      this.peerId = verifyHelloIdentity(header, this.endpoint, challengeBytes);
      this.helloResponse = header;
      return header;
    })();
    try {
      return await this.helloPromise;
    } finally {
      this.helloPromise = undefined;
    }
  }

  async findNode(target, count = 20) {
    hexToBytes(target, 32);
    const { header } = await this.request("find_node", { target, count });
    if (header.type !== "nodes") throw new Error("Expected a NODES response");
    if (header.target.toLowerCase() !== target.toLowerCase()) {
      throw new Error("Node returned results for a different lookup target");
    }
    if (!Array.isArray(header.nodes)) {
      throw new Error("Node returned an invalid node list");
    }
    for (const node of header.nodes) {
      hexToBytes(node.peer_id, 32);
      if (node.webrtc_direct) {
        const parsed = normalizeEndpoint(node.webrtc_direct);
        if (parsed.peerId !== node.peer_id.toLowerCase()) {
          throw new Error(
            `Node ${node.peer_id} advertised another peer's endpoint`,
          );
        }
      }
    }
    return header.nodes;
  }

  async getChunk(address) {
    hexToBytes(address, 32);
    const response = await this.request("get_chunk", { address });
    if (response.header.status === "not_found") {
      const error = new Error(`Chunk ${address} was not found on this node`);
      error.code = "not_found";
      throw error;
    }
    if (response.header.type !== "chunk") {
      throw new Error("Expected a CHUNK response");
    }
    if (response.header.address.toLowerCase() !== address.toLowerCase()) {
      throw new Error("Node returned a different chunk address");
    }
    if (response.header.size !== response.content.length) {
      throw new Error("Chunk metadata size does not match its content");
    }
    const hash = verifyChunk(address, response.content);
    return { content: response.content, hash };
  }

  async quoteChunk(address, size) {
    hexToBytes(address, 32);
    if (!Number.isSafeInteger(size) || size < 0 || size > MAX_CHUNK_SIZE) {
      throw new Error(`Invalid chunk size ${size}`);
    }
    const { header } = await this.request("quote_chunk", { address, size });
    if (header.type !== "storage_quote") {
      throw new Error("Expected a STORAGE_QUOTE response");
    }
    if (header.address.toLowerCase() !== address.toLowerCase()) {
      throw new Error("Node returned a quote for a different chunk address");
    }
    return {
      quote: header.quote,
      alreadyStored: Boolean(header.already_stored),
    };
  }

  async putChunk(address, content, quote, transactionHash) {
    hexToBytes(address, 32);
    hexToBytes(transactionHash, 32);
    verifyChunk(address, content);
    const { header } = await this.request(
      "put_chunk",
      { address, quote, transaction_hash: transactionHash },
      content,
    );
    if (header.type !== "chunk_stored") {
      throw new Error("Expected a CHUNK_STORED response");
    }
    if (header.address.toLowerCase() !== address.toLowerCase()) {
      throw new Error("Node stored a different chunk address");
    }
    return {
      address: header.address,
      alreadyStored: Boolean(header.already_stored),
    };
  }

  close() {
    this.dataChannel?.close();
    this.peerConnection?.close();
    this.dataChannel = undefined;
    this.peerConnection = undefined;
    this.inbox = undefined;
    this.connectPromise = undefined;
    this.peerId = undefined;
    this.helloResponse = undefined;
    this.helloPromise = undefined;
  }
}

function endpointKey(endpoint) {
  const normalized = normalizeEndpoint(endpoint);
  return normalized.multiaddr;
}

const DEFAULT_MAX_POOLED_CLIENTS = 10;

/**
 * A bounded set of reusable browser-to-node WebRTC associations.
 *
 * In Safari, the PoC exhausted WebRTC resources when it rapidly replaced every
 * seed connection for each chunk, even though callers invoked `close()`.
 * Keeping authenticated, persistent DataChannels avoids relying on prompt
 * reclamation and avoids repeating ICE, DTLS, SCTP, and HELLO for each lookup
 * and record.
 */
export class BrowserNodeClientPool {
  constructor({
    maxClients = DEFAULT_MAX_POOLED_CLIENTS,
    clientFactory = (endpoint) => new BrowserNodeClient(endpoint),
  } = {}) {
    if (!Number.isSafeInteger(maxClients) || maxClients < 1) {
      throw new Error("WebRTC client pool size must be a positive integer");
    }
    if (typeof clientFactory !== "function") {
      throw new Error("WebRTC client pool factory must be a function");
    }
    this.maxClients = maxClients;
    this.clientFactory = clientFactory;
    this.entries = new Map();
    this.waiters = [];
    this.clock = 0;
    this.closed = false;
  }

  get size() {
    return this.entries.size;
  }

  async withClient(endpoint, operation) {
    if (typeof operation !== "function") {
      throw new Error("WebRTC client pool operation must be a function");
    }
    const entry = await this.acquire(endpoint);
    try {
      return await operation(entry.client);
    } finally {
      this.release(entry);
    }
  }

  async acquire(endpoint) {
    const key = endpointKey(endpoint);
    for (;;) {
      if (this.closed) throw new Error("WebRTC client pool is closed");

      const existing = this.entries.get(key);
      if (existing) {
        existing.active += 1;
        existing.lastUsed = ++this.clock;
        return existing;
      }

      if (this.entries.size < this.maxClients) {
        const entry = {
          key,
          client: this.clientFactory(endpoint),
          active: 1,
          lastUsed: ++this.clock,
        };
        this.entries.set(key, entry);
        return entry;
      }

      let oldestIdle;
      for (const candidate of this.entries.values()) {
        if (
          candidate.active === 0 &&
          (!oldestIdle || candidate.lastUsed < oldestIdle.lastUsed)
        ) {
          oldestIdle = candidate;
        }
      }
      if (oldestIdle) {
        this.entries.delete(oldestIdle.key);
        oldestIdle.client.close();
        continue;
      }

      await new Promise((resolve) => this.waiters.push(resolve));
    }
  }

  release(entry) {
    entry.active = Math.max(0, entry.active - 1);
    entry.lastUsed = ++this.clock;
    this.waiters.shift()?.();
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    for (const entry of this.entries.values()) entry.client.close();
    this.entries.clear();
    for (const wake of this.waiters.splice(0)) wake();
  }
}

export async function iterativeFindClosest(
  seedEndpoints,
  target,
  {
    k = 20,
    alpha = 3,
    maxIterations = 20,
    onProgress = () => {},
    clientPool,
  } = {},
) {
  hexToBytes(target, 32);
  if (!Array.isArray(seedEndpoints) || seedEndpoints.length === 0) {
    throw new Error("At least one seed endpoint is required");
  }

  const ownsClientPool = clientPool === undefined;
  const pool = clientPool ?? new BrowserNodeClientPool();
  const failures = [];
  const seedNodes = [];

  await Promise.all(
    seedEndpoints.map(async (endpoint) => {
      const seedName =
        typeof endpoint === "string"
          ? endpoint
          : (endpoint?.multiaddr ?? "seed");
      try {
        const hello = await pool.withClient(endpoint, (client) =>
          client.hello(),
        );
        seedNodes.push({
          peer_id: hello.peer_id,
          native_addresses: [],
          reliability: 1,
          webrtc_direct: hello.endpoint,
        });
        onProgress(`Connected seed ${hello.peer_id}`);
      } catch (error) {
        failures.push({ peerId: seedName, error });
        onProgress(`Seed ${seedName} failed: ${error.message}`);
      }
    }),
  );
  if (seedNodes.length === 0) {
    if (ownsClientPool) pool.close();
    const detail = failures.map(({ error }) => error.message).join("; ");
    throw new Error(`Could not connect to any WebRtcDirect seed: ${detail}`);
  }

  const lookup = new BrowserIterativeLookupNative(
    target,
    k,
    alpha,
    maxIterations,
  );
  lookup.addCandidates(seedNodes);
  try {
    await lookup.run(
      async ({ target: lookupTarget, count, iteration, candidates }) =>
        Promise.all(
          candidates.map(async (candidate) => {
            try {
              const nodes = await pool.withClient(
                candidate.webrtc_direct,
                async (client) => {
                  if (!client.peerId) await client.hello();
                  return client.findNode(lookupTarget, count);
                },
              );
              onProgress(
                `Iteration ${iteration}: ${candidate.peer_id} returned ${nodes.length} nodes`,
              );
              return {
                status: "succeeded",
                responder: candidate.peer_id,
                candidates: nodes,
              };
            } catch (error) {
              failures.push({ peerId: candidate.peer_id, error });
              onProgress(`Query ${candidate.peer_id} failed: ${error.message}`);
              return { status: "failed", responder: candidate.peer_id };
            }
          }),
        ),
    );

    return {
      nodes: lookup.results(),
      queried: lookup.queriedPeers(),
      failures,
      clientPool: pool,
      ownsClientPool,
    };
  } catch (error) {
    if (ownsClientPool) pool.close();
    throw error;
  }
}

export async function getChunkFromClosest(
  seedEndpoints,
  address,
  { onProgress = () => {}, ...lookupOptions } = {},
) {
  hexToBytes(address, 32);
  const lookup = await iterativeFindClosest(seedEndpoints, address, {
    ...lookupOptions,
    onProgress,
  });
  const attempted = [];

  try {
    for (const node of lookup.nodes) {
      if (!node.webrtc_direct) continue;
      try {
        onProgress(`Requesting ${address} from ${node.peer_id}`);
        const chunk = await lookup.clientPool.withClient(
          node.webrtc_direct,
          async (client) => {
            if (!client.peerId) await client.hello();
            return client.getChunk(address);
          },
        );
        return { ...chunk, node, lookup };
      } catch (error) {
        attempted.push({ peerId: node.peer_id, error });
        onProgress(
          `Node ${node.peer_id} did not return the file: ${error.message}`,
        );
      }
    }
  } finally {
    if (lookup.ownsClientPool) lookup.clientPool.close();
  }

  const detail = attempted
    .map(({ peerId, error }) => `${peerId}: ${error.message}`)
    .join("; ");
  throw new Error(
    `No closest WebRtcDirect node returned chunk ${address}${detail ? ` (${detail})` : ""}`,
  );
}
