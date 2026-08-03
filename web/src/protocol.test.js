import assert from "node:assert/strict";
import test from "node:test";
import { blake3 } from "@noble/hashes/blake3.js";
import {
  bytesToHex,
  getChunkFromClosest,
  hexToBytes,
  parseResponseFrame,
  verifyChunk,
  xorDistance,
} from "./protocol.js";

test("hex conversion enforces fixed widths", () => {
  const value = "ab".repeat(32);
  assert.equal(bytesToHex(hexToBytes(value, 32)), value);
  assert.throws(() => hexToBytes("abcd", 32), /Expected 32 bytes/);
  assert.throws(() => hexToBytes("zz", 1), /hexadecimal/);
});

test("XOR distance is an unsigned 256-bit ordering value", () => {
  assert.equal(xorDistance("00".repeat(32), "00".repeat(32)), 0n);
  assert.equal(xorDistance("00".repeat(31) + "01", "00".repeat(32)), 1n);
  assert(xorDistance("80" + "00".repeat(31), "00".repeat(32)) > 1n);
});

test("response framing preserves a raw binary body", () => {
  const header = new TextEncoder().encode(
    JSON.stringify({
      version: 1,
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

test("BLAKE3 verification accepts the canonical empty hash", () => {
  const emptyHash =
    "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
  assert.equal(verifyChunk(emptyHash, new Uint8Array()), emptyHash);
  assert.throws(() => verifyChunk("00".repeat(32), new Uint8Array()), /BLAKE3 mismatch/);
});

test("browser lookup discovers a direct node and downloads a verified chunk", async (t) => {
  const content = new TextEncoder().encode("direct browser test file");
  const address = bytesToHex(blake3(content));
  const seedPeer = "ff".repeat(32);
  const storagePeer = address;
  const seed = {
    peer_id: seedPeer,
    url: "https://seed.test/autonomi/webtransport/v1",
    certificate_sha256: "11".repeat(32),
  };
  const storage = {
    peer_id: storagePeer,
    url: "https://storage.test/autonomi/webtransport/v1",
    certificate_sha256: "22".repeat(32),
  };
  const calls = [];
  const routes = new Map([
    [
      seed.url,
      (request) => {
        calls.push(["seed", request.type]);
        if (request.type === "hello") return helloResponse(request, seed);
        if (request.type === "find_node") {
          return response(request, {
            type: "nodes",
            target: request.target,
            nodes: [browserNode(storage), browserNode(seed)],
          });
        }
        throw new Error(`Unexpected seed request ${request.type}`);
      },
    ],
    [
      storage.url,
      (request) => {
        calls.push(["storage", request.type]);
        if (request.type === "hello") return helloResponse(request, storage);
        if (request.type === "find_node") {
          return response(request, {
            type: "nodes",
            target: request.target,
            nodes: [browserNode(storage), browserNode(seed)],
          });
        }
        if (request.type === "get_chunk") {
          return response(
            request,
            { type: "chunk", address: request.address, size: content.length },
            content,
          );
        }
        throw new Error(`Unexpected storage request ${request.type}`);
      },
    ],
  ]);

  const previousWebTransport = globalThis.WebTransport;
  globalThis.WebTransport = mockWebTransport(routes);
  t.after(() => {
    globalThis.WebTransport = previousWebTransport;
  });

  const downloaded = await getChunkFromClosest([seed], address);
  assert.deepEqual(downloaded.content, content);
  assert.equal(downloaded.hash, address);
  assert.equal(downloaded.node.peer_id, storagePeer);
  assert.deepEqual(calls, [
    ["seed", "hello"],
    ["seed", "find_node"],
    ["storage", "hello"],
    ["storage", "find_node"],
    ["storage", "get_chunk"],
  ]);
});

function browserNode(endpoint) {
  return {
    peer_id: endpoint.peer_id,
    native_addresses: [],
    reliability: 1,
    webtransport: {
      url: endpoint.url,
      certificate_sha256: endpoint.certificate_sha256,
    },
  };
}

function helloResponse(request, endpoint) {
  return response(request, {
    type: "hello",
    protocol: "autonomi.web.poc.v1",
    peer_id: endpoint.peer_id,
    max_chunk_size: 4 * 1024 * 1024,
    endpoint: {
      url: endpoint.url,
      certificate_sha256: endpoint.certificate_sha256,
    },
    capabilities: ["find_node", "get_chunk"],
  });
}

function response(request, fields, content = new Uint8Array()) {
  return {
    header: {
      version: 1,
      request_id: request.request_id,
      status: "ok",
      content_length: content.length,
      ...fields,
    },
    content,
  };
}

function encodeResponse({ header, content }) {
  const headerBytes = new TextEncoder().encode(JSON.stringify(header));
  const frame = new Uint8Array(4 + headerBytes.length + content.length);
  new DataView(frame.buffer).setUint32(0, headerBytes.length, false);
  frame.set(headerBytes, 4);
  frame.set(content, 4 + headerBytes.length);
  return frame;
}

function mockWebTransport(routes) {
  return class MockWebTransport {
    constructor(url, options) {
      this.url = url;
      this.options = options;
      this.ready = routes.has(url)
        ? Promise.resolve()
        : Promise.reject(new Error(`No mock endpoint for ${url}`));
    }

    async createBidirectionalStream() {
      const requestChunks = [];
      let responseController;
      const readable = new ReadableStream({
        start(controller) {
          responseController = controller;
        },
      });
      const writable = new WritableStream({
        write(chunk) {
          requestChunks.push(chunk);
        },
        close: () => {
          try {
            const length = requestChunks.reduce((total, chunk) => total + chunk.length, 0);
            const encoded = new Uint8Array(length);
            let offset = 0;
            for (const chunk of requestChunks) {
              encoded.set(chunk, offset);
              offset += chunk.length;
            }
            const request = JSON.parse(new TextDecoder().decode(encoded));
            const handler = routes.get(this.url);
            responseController.enqueue(encodeResponse(handler(request)));
            responseController.close();
          } catch (error) {
            responseController.error(error);
          }
        },
      });
      return { readable, writable };
    }

    close() {}
  };
}
