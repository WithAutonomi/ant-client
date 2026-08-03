import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { blake3 } from "@noble/hashes/blake3.js";
import { bytesToHex, getChunkFromClosest, hexToBytes } from "./protocol.js";

const KDF_CONTEXT = new TextEncoder().encode("self_encryption/chunk/v2");
const PAD_SIZE = 52;
const KEY_SIZE = 32;
const NONCE_SIZE = 12;
const DERIVED_SIZE = PAD_SIZE + KEY_SIZE + NONCE_SIZE;
const MAX_DOWNLOAD_CONCURRENCY = 6;

function writeU64LittleEndian(target, offset, value) {
  const numeric = BigInt(value);
  new DataView(target.buffer, target.byteOffset, target.byteLength).setBigUint64(
    offset,
    numeric,
    true,
  );
}

function predecessorIndices(index, count) {
  if (!Number.isSafeInteger(index) || index < 0 || index >= count || count < 3) {
    throw new Error(`Invalid self-encryption chunk index ${index}/${count}`);
  }
  if (index === 0) return [count - 1, count - 2];
  if (index === 1) return [0, count - 1];
  return [index - 1, index - 2];
}

export function deriveChunkMaterial(chunk, sourceHashes, childLevel = 0) {
  const [previous, previousPrevious] = predecessorIndices(chunk.index, sourceHashes.length);
  const context = new Uint8Array(32 * 3 + 8 * 2);
  context.set(sourceHashes[chunk.index], 0);
  context.set(sourceHashes[previous], 32);
  context.set(sourceHashes[previousPrevious], 64);
  writeU64LittleEndian(context, 96, chunk.index);
  writeU64LittleEndian(context, 104, childLevel);

  const derived = blake3(context, { context: KDF_CONTEXT, dkLen: DERIVED_SIZE });
  return {
    pad: derived.slice(0, PAD_SIZE),
    key: derived.slice(PAD_SIZE, PAD_SIZE + KEY_SIZE),
    nonce: derived.slice(PAD_SIZE + KEY_SIZE),
  };
}

export async function decryptSelfEncryptedChunk(
  chunk,
  encryptedContent,
  sourceHashes,
  childLevel = 0,
  decompress = decompressBrotli,
) {
  const { pad, key, nonce } = deriveChunkMaterial(chunk, sourceHashes, childLevel);
  const ciphertext = new Uint8Array(encryptedContent.length);
  for (let index = 0; index < encryptedContent.length; index += 1) {
    ciphertext[index] = encryptedContent[index] ^ pad[index % pad.length];
  }

  let compressed;
  try {
    compressed = chacha20poly1305(key, nonce).decrypt(ciphertext);
  } catch (error) {
    throw new Error(`Chunk ${chunk.index} authentication failed`, { cause: error });
  }

  let plaintext;
  try {
    plaintext = await decompress(compressed);
  } catch (error) {
    throw new Error(`Chunk ${chunk.index} Brotli decompression failed`, { cause: error });
  }
  if (plaintext.length !== chunk.src_size) {
    throw new Error(
      `Chunk ${chunk.index} reconstructed ${plaintext.length} bytes, expected ${chunk.src_size}`,
    );
  }
  const sourceHash = bytesToHex(blake3(plaintext));
  if (sourceHash !== chunk.src_hash) {
    throw new Error(
      `Chunk ${chunk.index} plaintext hash mismatch: expected ${chunk.src_hash}, received ${sourceHash}`,
    );
  }
  return plaintext;
}

async function decompressBrotli(compressed) {
  const { default: brotliPromise } = await import("brotli-dec-wasm");
  const brotli = await brotliPromise;
  return brotli.decompress(compressed);
}

async function mapWithConcurrency(items, concurrency, operation) {
  const results = new Array(items.length);
  let next = 0;
  const workerCount = Math.min(items.length, concurrency);
  await Promise.all(
    Array.from({ length: workerCount }, async () => {
      while (next < items.length) {
        const index = next;
        next += 1;
        results[index] = await operation(items[index], index);
      }
    }),
  );
  return results;
}

export async function downloadPublicFile(
  seedEndpoints,
  file,
  {
    concurrency = 3,
    onProgress = () => {},
    downloadChunk = getChunkFromClosest,
    decompress = decompressBrotli,
  } = {},
) {
  hexToBytes(file.address, 32);
  hexToBytes(file.blake3, 32);
  if (!Number.isSafeInteger(concurrency) || concurrency < 1) {
    throw new Error("Download concurrency must be a positive integer");
  }
  const boundedConcurrency = Math.min(concurrency, MAX_DOWNLOAD_CONCURRENCY);

  onProgress(`Fetching public DataMap ${file.address}`);
  const dataMap = await downloadChunk(seedEndpoints, file.address, { onProgress });
  if (dataMap.content.length !== file.data_map_size) {
    throw new Error(
      `Public DataMap has ${dataMap.content.length} bytes, expected ${file.data_map_size}`,
    );
  }
  onProgress(`Verified public DataMap (${dataMap.content.length} bytes)`);

  const sourceHashes = file.chunks.map((chunk) => hexToBytes(chunk.src_hash, 32));
  const plaintextChunks = await mapWithConcurrency(
    file.chunks,
    boundedConcurrency,
    async (chunk) => {
      onProgress(
        `Fetching encrypted file chunk ${chunk.index + 1}/${file.chunks.length} (${chunk.dst_hash})`,
      );
      const downloaded = await downloadChunk(seedEndpoints, chunk.dst_hash, {
        onProgress,
      });
      const plaintext = await decryptSelfEncryptedChunk(
        chunk,
        downloaded.content,
        sourceHashes,
        0,
        decompress,
      );
      onProgress(`Reconstructed file chunk ${chunk.index + 1}/${file.chunks.length}`);
      return plaintext;
    },
  );

  const totalSize = plaintextChunks.reduce((total, chunk) => total + chunk.length, 0);
  if (totalSize !== file.size) {
    throw new Error(`Reconstructed file has ${totalSize} bytes, expected ${file.size}`);
  }
  const content = new Uint8Array(totalSize);
  let offset = 0;
  for (const chunk of plaintextChunks) {
    content.set(chunk, offset);
    offset += chunk.length;
  }
  const hash = bytesToHex(blake3(content));
  if (hash !== file.blake3) {
    throw new Error(`Whole-file BLAKE3 mismatch: expected ${file.blake3}, received ${hash}`);
  }
  onProgress(`Verified complete ${file.name} as ${hash}`);
  return { content, hash, dataMapNode: dataMap.node };
}
