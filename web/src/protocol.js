import { blake3 } from "@noble/hashes/blake3.js";
import { bytesToHex as nobleBytesToHex } from "@noble/hashes/utils.js";

export const PROTOCOL_VERSION = 1;
export const PROTOCOL_NAME = "autonomi.web.poc.v1";
export const MAX_CHUNK_SIZE = 4 * 1024 * 1024;
export const MAX_RESPONSE_HEADER_BYTES = 64 * 1024;
const MAX_RESPONSE_BYTES = 4 + MAX_RESPONSE_HEADER_BYTES + MAX_CHUNK_SIZE;
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

export function xorDistance(peerId, target) {
  const peer = hexToBytes(peerId, 32);
  const key = hexToBytes(target, 32);
  let distance = 0n;
  for (let index = 0; index < peer.length; index += 1) {
    distance = (distance << 8n) | BigInt(peer[index] ^ key[index]);
  }
  return distance;
}

export function verifyChunk(address, content) {
  const expected = address.trim().replace(/^0x/i, "").toLowerCase();
  hexToBytes(expected, 32);
  const actual = nobleBytesToHex(blake3(content));
  if (actual !== expected) {
    throw new Error(`BLAKE3 mismatch: expected ${expected}, received ${actual}`);
  }
  return actual;
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

function normalizeEndpoint(endpoint) {
  if (!endpoint || typeof endpoint.url !== "string") {
    throw new Error("Endpoint URL is required");
  }
  const certificateSha256 =
    endpoint.certificate_sha256 ?? endpoint.certificateSha256;
  return {
    url: endpoint.url,
    peerId: endpoint.peer_id ?? endpoint.peerId,
    certificateSha256,
    certificateBytes: hexToBytes(certificateSha256 ?? "", 32),
  };
}

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
      serverCertificateHashes: [
        { algorithm: "sha-256", value: this.endpoint.certificateBytes },
      ],
    });
    try {
      await transport.ready;
    } catch (error) {
      transport.close();
      throw error;
    }
    this.transport = transport;
  }

  async request(type, fields = {}) {
    await this.connect();
    const requestId = nextRequestId;
    nextRequestId += 1;
    const stream = await this.transport.createBidirectionalStream();
    const writer = stream.writable.getWriter();
    try {
      await writer.write(
        encoder.encode(
          JSON.stringify({
            version: PROTOCOL_VERSION,
            request_id: requestId,
            type,
            ...fields,
          }),
        ),
      );
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
    const advertisedEndpoint = normalizeEndpoint(header.endpoint);
    if (
      advertisedEndpoint.url !== this.endpoint.url ||
      advertisedEndpoint.certificateSha256.toLowerCase() !==
        this.endpoint.certificateSha256.toLowerCase()
    ) {
      throw new Error("Node advertised a different WebTransport endpoint");
    }
    hexToBytes(header.peer_id, 32);
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
    for (const node of header.nodes) hexToBytes(node.peer_id, 32);
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

  close() {
    this.transport?.close({ closeCode: 0, reason: "client closed" });
    this.transport = undefined;
  }
}

function endpointKey(endpoint) {
  const normalized = normalizeEndpoint(endpoint);
  return `${normalized.url}|${normalized.certificateSha256}`;
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
  const known = new Map();
  const queried = new Set();
  const failures = [];

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
      const seedName = endpoint?.peer_id ?? endpoint?.peerId ?? endpoint?.url ?? "seed";
      try {
        const client = clientFor(endpoint);
        const hello = await client.hello();
        known.set(hello.peer_id, {
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
  if (known.size === 0) {
    for (const client of clients.values()) client.close();
    const detail = failures.map(({ error }) => error.message).join("; ");
    throw new Error(`Could not connect to any WebTransport seed: ${detail}`);
  }

  for (let iteration = 0; iteration < maxIterations; iteration += 1) {
    const ordered = [...known.values()].sort((left, right) => {
      const a = xorDistance(left.peer_id, target);
      const b = xorDistance(right.peer_id, target);
      return a < b ? -1 : a > b ? 1 : 0;
    });
    const candidates = ordered
      .filter((node) => node.webtransport && !queried.has(node.peer_id))
      .slice(0, alpha);
    if (candidates.length === 0) break;

    let discovered = 0;
    await Promise.all(
      candidates.map(async (candidate) => {
        queried.add(candidate.peer_id);
        try {
          const candidateClient = clientFor({
            ...candidate.webtransport,
            peer_id: candidate.peer_id,
          });
          if (!candidateClient.peerId) await candidateClient.hello();
          const nodes = await candidateClient.findNode(target, k);
          onProgress(
            `Iteration ${iteration + 1}: ${candidate.peer_id} returned ${nodes.length} nodes`,
          );
          for (const node of nodes) {
            const existing = known.get(node.peer_id);
            if (!existing) discovered += 1;
            known.set(node.peer_id, {
              ...existing,
              ...node,
              webtransport: node.webtransport ?? existing?.webtransport,
            });
          }
        } catch (error) {
          failures.push({ peerId: candidate.peer_id, error });
          onProgress(`Query ${candidate.peer_id} failed: ${error.message}`);
        }
      }),
    );

    const remainingQueryable = [...known.values()]
      .sort((left, right) => {
        const a = xorDistance(left.peer_id, target);
        const b = xorDistance(right.peer_id, target);
        return a < b ? -1 : a > b ? 1 : 0;
      })
      .slice(0, k)
      .some((node) => node.webtransport && !queried.has(node.peer_id));
    if (discovered === 0 && !remainingQueryable) break;
  }

  const nodes = [...known.values()]
    .sort((left, right) => {
      const a = xorDistance(left.peer_id, target);
      const b = xorDistance(right.peer_id, target);
      return a < b ? -1 : a > b ? 1 : 0;
    })
    .slice(0, k);

  return { nodes, queried: [...queried], failures, clients };
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
      const endpoint = { ...node.webtransport, peer_id: node.peer_id };
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
