import { bytesToHex as nobleBytesToHex } from "@noble/hashes/utils.js";
import {
  BrowserIterativeLookup as BrowserIterativeLookupNative,
  verifyRecord as verifyRecordNative,
} from "../pkg/ant_core.js";

export const PROTOCOL_VERSION = 3;
export const PROTOCOL_NAME = "autonomi.web.poc.v3";
export const WEBTRANSPORT_PATH = "/autonomi/webtransport/v1";
export const MAX_CHUNK_SIZE = 4 * 1024 * 1024;
export const MAX_RESPONSE_HEADER_BYTES = 64 * 1024;
const MAX_RESPONSE_BYTES = 4 + MAX_RESPONSE_HEADER_BYTES + MAX_CHUNK_SIZE;
const MAX_WEBTRANSPORT_MULTIADDR_LENGTH = 2048;
const MAX_CERTIFICATE_HASHES = 2;
const SHA2_256_MULTIHASH_CODE = 0x12;
const SHA2_256_MULTIHASH_LENGTH = 32;
const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

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
    throw new Error(`Expected ${expectedLength} bytes, received ${bytes.length}`);
  }
  return bytes;
}

export function bytesToHex(bytes) {
  return nobleBytesToHex(bytes);
}

export function verifyChunk(address, content) {
  const expected = address.trim().replace(/^0x/i, "").toLowerCase();
  hexToBytes(expected, 32);
  return verifyRecordNative(expected, content);
}

export function parseResponseFrame(frame) {
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
    throw new Error(`Invalid response JSON: ${error.message}`, { cause: error });
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
  if (frame.length !== contentOffset + header.content_length) {
    throw new Error(
      `Response length mismatch: declared ${header.content_length} content bytes`,
    );
  }
  return { header, content: frame.slice(contentOffset) };
}

async function readAll(readable, limit = MAX_RESPONSE_BYTES) {
  const reader = readable.getReader();
  const chunks = [];
  let total = 0;
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      if (!(value instanceof Uint8Array)) {
        throw new Error("WebTransport returned a non-byte stream chunk");
      }
      total += value.length;
      if (total > limit) {
        await reader.cancel("response exceeded client limit");
        throw new Error(`Response exceeded the ${limit}-byte client limit`);
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }

  const result = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.length;
  }
  return result;
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
    throw new Error("Certificate multihash must contain a 32-byte SHA-256 digest");
  }
  return decoded.slice(2);
}

function validateIpv4(value) {
  const octets = value.split(".");
  if (
    octets.length !== 4 ||
    octets.some(
      (octet) =>
        !/^(0|[1-9][0-9]{0,2})$/.test(octet) || Number.parseInt(octet, 10) > 255,
    )
  ) {
    throw new Error(`Invalid IPv4 address ${value}`);
  }
  return value;
}

function endpointMultiaddr(endpoint) {
  if (typeof endpoint === "string") return endpoint;
  if (endpoint && typeof endpoint.multiaddr === "string") return endpoint.multiaddr;
  throw new Error("A WebTransport multiaddress is required");
}

export function parseWebTransportMultiaddr(endpoint) {
  const multiaddr = endpointMultiaddr(endpoint).trim();
  if (
    multiaddr.length === 0 ||
    multiaddr.length > MAX_WEBTRANSPORT_MULTIADDR_LENGTH ||
    !multiaddr.startsWith("/")
  ) {
    throw new Error("Invalid WebTransport multiaddress length or prefix");
  }
  const segments = multiaddr.split("/");
  if (segments.length < 9) {
    throw new Error("WebTransport multiaddress is incomplete");
  }

  const hostProtocol = segments[1];
  const hostValue = segments[2];
  if (!hostValue) throw new Error("WebTransport multiaddress host is empty");
  let urlHost;
  if (hostProtocol === "ip4") {
    urlHost = validateIpv4(hostValue);
  } else if (hostProtocol === "ip6") {
    urlHost = `[${hostValue}]`;
  } else if (["dns", "dns4", "dns6"].includes(hostProtocol)) {
    urlHost = hostValue.toLowerCase();
  } else {
    throw new Error(`Unsupported WebTransport host protocol ${hostProtocol}`);
  }
  if (segments[3] !== "udp") {
    throw new Error("WebTransport multiaddress must use UDP");
  }
  if (!/^[0-9]{1,5}$/.test(segments[4])) {
    throw new Error("WebTransport multiaddress has an invalid UDP port");
  }
  const port = Number.parseInt(segments[4], 10);
  if (port < 1 || port > 65535) {
    throw new Error("WebTransport multiaddress has an invalid UDP port");
  }
  if (segments[5] !== "quic-v1" || segments[6] !== "webtransport") {
    throw new Error(
      "WebTransport multiaddress must contain /quic-v1/webtransport",
    );
  }

  let index = 7;
  const certificateHashes = [];
  const certificateHashMultihashes = [];
  while (segments[index] === "certhash") {
    const encoded = segments[index + 1];
    if (!encoded) throw new Error("WebTransport multiaddress has an empty certhash");
    certificateHashes.push(decodeCertificateMultihash(encoded));
    certificateHashMultihashes.push(encoded);
    index += 2;
  }
  if (
    certificateHashes.length < 1 ||
    certificateHashes.length > MAX_CERTIFICATE_HASHES
  ) {
    throw new Error(
      `WebTransport multiaddress must contain between 1 and ${MAX_CERTIFICATE_HASHES} certificate hashes`,
    );
  }
  if (new Set(certificateHashMultihashes).size !== certificateHashes.length) {
    throw new Error("WebTransport multiaddress contains duplicate certificate hashes");
  }
  if (segments[index] !== "p2p" || index + 2 !== segments.length) {
    throw new Error("WebTransport multiaddress must end with /p2p/<peer-id>");
  }
  const peerId = segments[index + 1]?.toLowerCase() ?? "";
  hexToBytes(peerId, 32);

  let url;
  try {
    url = new URL(`https://${urlHost}:${port}${WEBTRANSPORT_PATH}`).toString();
  } catch (error) {
    throw new Error("WebTransport multiaddress contains an invalid host", {
      cause: error,
    });
  }
  return {
    multiaddr,
    url,
    peerId,
    certificateHashes,
  };
}

const normalizeEndpoint = parseWebTransportMultiaddr;

export class BrowserNodeClient {
  constructor(endpoint) {
    this.endpoint = normalizeEndpoint(endpoint);
    this.transport = undefined;
    this.peerId = undefined;
  }

  async connect() {
    if (this.transport) return;
    if (typeof WebTransport === "undefined") {
      throw new Error("This browser does not expose the WebTransport API");
    }
    const transport = new WebTransport(this.endpoint.url, {
      serverCertificateHashes: this.endpoint.certificateHashes.map((value) => ({
        algorithm: "sha-256",
        value,
      })),
    });
    try {
      await transport.ready;
    } catch (error) {
      transport.close();
      throw error;
    }
    this.transport = transport;
  }

  async request(type, fields = {}, content = new Uint8Array()) {
    await this.connect();
    if (!(content instanceof Uint8Array) || content.length > MAX_CHUNK_SIZE) {
      throw new Error(`Request content must be at most ${MAX_CHUNK_SIZE} bytes`);
    }
    const requestId = nextRequestId;
    nextRequestId += 1;
    const stream = await this.transport.createBidirectionalStream();
    const writer = stream.writable.getWriter();
    try {
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
      const prefix = new Uint8Array(4);
      new DataView(prefix.buffer).setUint32(0, header.length, false);
      await writer.write(prefix);
      await writer.write(header);
      if (content.length > 0) await writer.write(content);
      await writer.close();
    } finally {
      writer.releaseLock();
    }

    const response = parseResponseFrame(await readAll(stream.readable));
    if (response.header.request_id !== requestId) {
      throw new Error(
        `Response ID ${response.header.request_id} does not match request ${requestId}`,
      );
    }
    if (response.header.status === "error") {
      const error = new Error(response.header.message ?? "Node returned an error");
      error.code = response.header.code;
      throw error;
    }
    return response;
  }

  async hello() {
    const { header } = await this.request("hello");
    if (header.type !== "hello") throw new Error("Expected a HELLO response");
    if (header.protocol !== PROTOCOL_NAME) {
      throw new Error(`Unsupported browser protocol ${header.protocol}`);
    }
    hexToBytes(header.peer_id, 32);
    const advertisedEndpoint = normalizeEndpoint(header.endpoint);
    if (
      advertisedEndpoint.multiaddr !== this.endpoint.multiaddr ||
      advertisedEndpoint.peerId !== header.peer_id.toLowerCase()
    ) {
      throw new Error("Node advertised a different WebTransport endpoint");
    }
    if (
      this.endpoint.peerId &&
      header.peer_id.toLowerCase() !== this.endpoint.peerId.toLowerCase()
    ) {
      throw new Error(
        `Endpoint identity mismatch: expected ${this.endpoint.peerId}, received ${header.peer_id}`,
      );
    }
    this.peerId = header.peer_id;
    return header;
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
      if (node.webtransport) {
        const parsed = normalizeEndpoint(node.webtransport);
        if (parsed.peerId !== node.peer_id.toLowerCase()) {
          throw new Error(`Node ${node.peer_id} advertised another peer's endpoint`);
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
    return { quote: header.quote, alreadyStored: Boolean(header.already_stored) };
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
    return { address: header.address, alreadyStored: Boolean(header.already_stored) };
  }

  close() {
    this.transport?.close({ closeCode: 0, reason: "client closed" });
    this.transport = undefined;
  }
}

function endpointKey(endpoint) {
  const normalized = normalizeEndpoint(endpoint);
  return normalized.multiaddr;
}

export async function iterativeFindClosest(
  seedEndpoints,
  target,
  { k = 20, alpha = 3, maxIterations = 20, onProgress = () => {} } = {},
) {
  hexToBytes(target, 32);
  if (!Array.isArray(seedEndpoints) || seedEndpoints.length === 0) {
    throw new Error("At least one seed endpoint is required");
  }

  const clients = new Map();
  const failures = [];
  const seedNodes = [];

  const clientFor = (endpoint) => {
    const key = endpointKey(endpoint);
    let client = clients.get(key);
    if (!client) {
      client = new BrowserNodeClient(endpoint);
      clients.set(key, client);
    }
    return client;
  };

  await Promise.all(
    seedEndpoints.map(async (endpoint) => {
      const seedName =
        typeof endpoint === "string" ? endpoint : endpoint?.multiaddr ?? "seed";
      try {
        const client = clientFor(endpoint);
        const hello = await client.hello();
        seedNodes.push({
          peer_id: hello.peer_id,
          native_addresses: [],
          reliability: 1,
          webtransport: hello.endpoint,
        });
        onProgress(`Connected seed ${hello.peer_id}`);
      } catch (error) {
        failures.push({ peerId: seedName, error });
        onProgress(`Seed ${seedName} failed: ${error.message}`);
      }
    }),
  );
  if (seedNodes.length === 0) {
    for (const client of clients.values()) client.close();
    const detail = failures.map(({ error }) => error.message).join("; ");
    throw new Error(`Could not connect to any WebTransport seed: ${detail}`);
  }

  const lookup = new BrowserIterativeLookupNative(target, k, alpha, maxIterations);
  lookup.addCandidates(seedNodes);
  await lookup.run(async ({ target: lookupTarget, count, iteration, candidates }) =>
    Promise.all(
      candidates.map(async (candidate) => {
        try {
          const candidateClient = clientFor(candidate.webtransport);
          if (!candidateClient.peerId) await candidateClient.hello();
          const nodes = await candidateClient.findNode(lookupTarget, count);
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

  return { nodes: lookup.results(), queried: lookup.queriedPeers(), failures, clients };
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
      if (!node.webtransport) continue;
      const endpoint = node.webtransport;
      const key = endpointKey(endpoint);
      let client = lookup.clients.get(key);
      if (!client) {
        client = new BrowserNodeClient(endpoint);
        lookup.clients.set(key, client);
      }

      try {
        if (!client.peerId) await client.hello();
        onProgress(`Requesting ${address} from ${node.peer_id}`);
        const chunk = await client.getChunk(address);
        return { ...chunk, node, lookup };
      } catch (error) {
        attempted.push({ peerId: node.peer_id, error });
        onProgress(`Node ${node.peer_id} did not return the file: ${error.message}`);
      }
    }
  } finally {
    for (const client of lookup.clients.values()) client.close();
  }

  const detail = attempted
    .map(({ peerId, error }) => `${peerId}: ${error.message}`)
    .join("; ");
  throw new Error(
    `No closest WebTransport node returned chunk ${address}${detail ? ` (${detail})` : ""}`,
  );
}
