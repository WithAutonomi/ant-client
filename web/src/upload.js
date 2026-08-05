import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { blake3 } from "@noble/hashes/blake3.js";
import { decode, encode } from "@msgpack/msgpack";
import { deriveChunkMaterial } from "./file.js";
import {
  BrowserNodeClient,
  bytesToHex,
  hexToBytes,
  iterativeFindClosest,
} from "./protocol.js";
import { payForStorageQuotes, verifyStorageQuote } from "./payment.js";

export const MAX_BROWSER_UPLOAD_BYTES = 64 * 1024 * 1024;
const SELF_ENCRYPTION_MAX_CHUNK_SIZE = 4_190_208;
const MAX_STORE_TARGETS = 7;

function numberOfChunks(fileSize) {
  if (fileSize < 3) return 0;
  if (fileSize < 3 * SELF_ENCRYPTION_MAX_CHUNK_SIZE) return 3;
  return Math.ceil(fileSize / SELF_ENCRYPTION_MAX_CHUNK_SIZE);
}

function chunkSize(fileSize, index) {
  if (fileSize < 3 * SELF_ENCRYPTION_MAX_CHUNK_SIZE) {
    return index < 2 ? Math.floor(fileSize / 3) : fileSize - 2 * Math.floor(fileSize / 3);
  }
  const count = numberOfChunks(fileSize);
  const remainder = fileSize % SELF_ENCRYPTION_MAX_CHUNK_SIZE;
  if (index < count - 2 || remainder === 0) return SELF_ENCRYPTION_MAX_CHUNK_SIZE;
  return index === count - 2 ? SELF_ENCRYPTION_MAX_CHUNK_SIZE : remainder;
}

function chunkStart(fileSize, index) {
  const count = numberOfChunks(fileSize);
  if (index === count - 1) {
    return chunkSize(fileSize, 0) * (index - 1) + chunkSize(fileSize, index - 1);
  }
  return chunkSize(fileSize, 0) * index;
}

async function defaultCompress(input) {
  const { default: brotliPromise } = await import("brotli-wasm");
  const brotli = await brotliPromise;
  return brotli.compress(input, { quality: 6 });
}

function xorPad(content, pad) {
  const output = new Uint8Array(content.length);
  for (let index = 0; index < content.length; index += 1) {
    output[index] = content[index] ^ pad[index % pad.length];
  }
  return output;
}

export function encodePublicDataMap(chunks) {
  const compact = [
    1,
    chunks.map((chunk) => [
      chunk.index,
      Array.from(hexToBytes(chunk.dst_hash, 32)),
      Array.from(hexToBytes(chunk.src_hash, 32)),
      chunk.src_size,
    ]),
    null,
  ];
  return encode(compact, { sortKeys: false });
}

function fixedBytes(value, length, label) {
  const bytes = value instanceof Uint8Array ? value : Uint8Array.from(value ?? []);
  if (bytes.length !== length) throw new Error(`${label} must contain ${length} bytes`);
  return bytes;
}

export function decodePublicDataMap(content) {
  let dataMap;
  try {
    dataMap = decode(content);
  } catch (error) {
    throw new Error(`Public DataMap is not valid MessagePack: ${error.message}`, {
      cause: error,
    });
  }
  if (!Array.isArray(dataMap) || dataMap.length !== 3 || dataMap[0] !== 1) {
    throw new Error("Public DataMap does not use self_encryption version 1");
  }
  if (dataMap[2] !== null) {
    throw new Error("Nested DataMaps are not yet supported by the browser uploader");
  }
  if (!Array.isArray(dataMap[1]) || dataMap[1].length < 3) {
    throw new Error("Public DataMap has fewer than three chunks");
  }
  const chunks = dataMap[1]
    .map((chunk) => {
      if (!Array.isArray(chunk) || chunk.length !== 4) {
        throw new Error("Public DataMap contains an invalid chunk descriptor");
      }
      const [index, dstHash, srcHash, srcSize] = chunk;
      if (!Number.isSafeInteger(index) || index < 0) {
        throw new Error(`Invalid DataMap chunk index ${index}`);
      }
      if (!Number.isSafeInteger(srcSize) || srcSize < 1) {
        throw new Error(`Invalid DataMap plaintext chunk size ${srcSize}`);
      }
      return {
        index,
        dst_hash: bytesToHex(fixedBytes(dstHash, 32, "DataMap destination hash")),
        src_hash: bytesToHex(fixedBytes(srcHash, 32, "DataMap source hash")),
        src_size: srcSize,
      };
    })
    .sort((left, right) => left.index - right.index);
  chunks.forEach((chunk, index) => {
    if (chunk.index !== index) throw new Error("DataMap chunk indices are not contiguous");
  });
  return chunks;
}

export async function encryptPublicFile(
  content,
  { name = "upload.bin", contentType = "application/octet-stream", compress = defaultCompress } = {},
) {
  if (!(content instanceof Uint8Array)) throw new Error("Upload content must be bytes");
  if (content.length < 3) throw new Error("Self-encryption requires a file of at least 3 bytes");
  if (content.length > MAX_BROWSER_UPLOAD_BYTES) {
    throw new Error(`Browser uploads are limited to ${MAX_BROWSER_UPLOAD_BYTES} bytes`);
  }
  const count = numberOfChunks(content.length);
  const plaintextChunks = Array.from({ length: count }, (_, index) => {
    const start = chunkStart(content.length, index);
    return content.slice(start, start + chunkSize(content.length, index));
  });
  const sourceHashes = plaintextChunks.map((chunk) => blake3(chunk));
  const encrypted = await Promise.all(
    plaintextChunks.map(async (plaintext, index) => {
      const descriptor = { index };
      const { pad, key, nonce } = deriveChunkMaterial(descriptor, sourceHashes, 0);
      const compressed = await compress(plaintext);
      const ciphertext = chacha20poly1305(key, nonce).encrypt(compressed);
      const bytes = xorPad(ciphertext, pad);
      return {
        content: bytes,
        info: {
          index,
          dst_hash: bytesToHex(blake3(bytes)),
          src_hash: bytesToHex(sourceHashes[index]),
          src_size: plaintext.length,
        },
      };
    }),
  );
  const chunks = encrypted.map(({ info }) => info);
  const dataMap = encodePublicDataMap(chunks);
  const address = bytesToHex(blake3(dataMap));
  const records = encrypted.map(({ content: bytes, info }) => ({
    address: info.dst_hash,
    content: bytes,
  }));
  records.push({ address, content: dataMap });
  return {
    descriptor: {
      name,
      address,
      size: content.length,
      content_type: contentType || "application/octet-stream",
      blake3: bytesToHex(blake3(content)),
      data_map_size: dataMap.length,
      chunks,
      replicas: 0,
    },
    records,
  };
}

function closeClients(clients) {
  for (const client of clients.values()) client.close();
}

function assertUploadNode(hello, paymentNetwork) {
  if (
    !Array.isArray(hello.capabilities) ||
    !hello.capabilities.includes("quote_chunk") ||
    !hello.capabilities.includes("put_chunk")
  ) {
    throw new Error("Node does not advertise paid browser uploads");
  }
  const advertised = hello.payment;
  if (
    !advertised ||
    new URL(advertised.rpc_url).toString() !== new URL(paymentNetwork.rpc_url).toString() ||
    advertised.payment_token_address?.toLowerCase() !==
      paymentNetwork.payment_token_address.toLowerCase() ||
    advertised.payment_vault_address?.toLowerCase() !==
      paymentNetwork.payment_vault_address.toLowerCase()
  ) {
    throw new Error("Node advertises a different payment network than the manifest");
  }
}

async function prepareRecord(seedEndpoints, paymentNetwork, record, onProgress) {
  onProgress(`Finding closest nodes for ${record.address}`);
  const lookup = await iterativeFindClosest(seedEndpoints, record.address, { onProgress });
  const endpoints = lookup.nodes
    .filter((node) => node.webtransport)
    .slice(0, MAX_STORE_TARGETS)
    .map((node) => ({ peerId: node.peer_id, endpoint: node.webtransport }));
  try {
    const failures = [];
    for (const target of endpoints) {
      const client = new BrowserNodeClient(target.endpoint);
      try {
        assertUploadNode(await client.hello(), paymentNetwork);
        const response = await client.quoteChunk(record.address, record.content.length);
        const verified = verifyStorageQuote(
          response.quote,
          record.address,
          target.peerId,
        );
        if (response.alreadyStored) {
          onProgress(`Chunk ${record.address} is already stored; skipping payment`);
          return { record, alreadyStored: true, targets: endpoints };
        }
        onProgress(`Verified storage quote ${verified.quoteHash} from ${target.peerId}`);
        return {
          record,
          alreadyStored: false,
          targets: [target, ...endpoints.filter((candidate) => candidate !== target)],
          verified,
        };
      } catch (error) {
        failures.push(`${target.peerId}: ${error.message}`);
      } finally {
        client.close();
      }
    }
    throw new Error(`No closest node supplied a valid quote (${failures.join("; ")})`);
  } finally {
    closeClients(lookup.clients);
  }
}

async function storePrepared(prepared, paymentNetwork, transactionHash, onProgress) {
  if (prepared.alreadyStored) return 1;
  const attempts = await Promise.allSettled(
    prepared.targets.map(async (target) => {
      const client = new BrowserNodeClient(target.endpoint);
      try {
        assertUploadNode(await client.hello(), paymentNetwork);
        const result = await client.putChunk(
          prepared.record.address,
          prepared.record.content,
          prepared.verified.quote,
          transactionHash,
        );
        onProgress(
          `${result.alreadyStored ? "Confirmed" : "Stored"} ${prepared.record.address} on ${target.peerId}`,
        );
        return result;
      } finally {
        client.close();
      }
    }),
  );
  const stored = attempts.filter((attempt) => attempt.status === "fulfilled").length;
  if (stored === 0) {
    const failures = attempts
      .filter((attempt) => attempt.status === "rejected")
      .map((attempt) => attempt.reason?.message ?? String(attempt.reason));
    throw new Error(`Paid chunk was rejected by every closest node: ${failures.join("; ")}`);
  }
  return stored;
}

export async function uploadPublicFile(
  seedEndpoints,
  paymentNetwork,
  file,
  walletSecret,
  { onProgress = () => {}, compress = defaultCompress } = {},
) {
  const content = new Uint8Array(await file.arrayBuffer());
  onProgress(`Self-encrypting ${file.name} (${content.length.toLocaleString()} bytes)`);
  const encrypted = await encryptPublicFile(content, {
    name: file.name,
    contentType: file.type,
    compress,
  });
  const prepared = [];
  for (let index = 0; index < encrypted.records.length; index += 1) {
    onProgress(`Preparing record ${index + 1}/${encrypted.records.length}`);
    prepared.push(
      await prepareRecord(seedEndpoints, paymentNetwork, encrypted.records[index], onProgress),
    );
  }
  const payable = prepared.filter((record) => !record.alreadyStored);
  let transactionHash;
  let totalAmount = 0n;
  if (payable.length > 0) {
    const payment = await payForStorageQuotes(
      paymentNetwork,
      payable.map((record) => record.verified),
      walletSecret,
      { onProgress },
    );
    transactionHash = payment.transactionHash;
    totalAmount = payment.totalAmount;
  }
  let replicas = Number.POSITIVE_INFINITY;
  for (let index = 0; index < prepared.length; index += 1) {
    onProgress(`Storing record ${index + 1}/${prepared.length}`);
    const stored = await storePrepared(
      prepared[index],
      paymentNetwork,
      transactionHash,
      onProgress,
    );
    replicas = Math.min(replicas, stored);
  }
  encrypted.descriptor.replicas = Number.isFinite(replicas) ? replicas : 0;
  return {
    file: encrypted.descriptor,
    transactionHash,
    storageCostAtto: totalAmount.toString(),
    records: encrypted.records.length,
  };
}
