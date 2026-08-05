import {
  decodePublicDataMap as decodePublicDataMapNative,
  decryptPublicFile as decryptPublicFileNative,
} from "../pkg/ant_core.js";
import { getChunkFromClosest, hexToBytes, verifyChunk } from "./protocol.js";

const MAX_DOWNLOAD_CONCURRENCY = 6;

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
    decodeDataMap = decodePublicDataMapNative,
    decrypt = decryptPublicFileNative,
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
  const chunks = decodeDataMap(dataMap.content);
  if (!Array.isArray(chunks) || chunks.length < 3) {
    throw new Error("ant-core WASM returned an invalid public DataMap");
  }

  const encryptedChunks = await mapWithConcurrency(
    chunks,
    boundedConcurrency,
    async (chunk) => {
      onProgress(
        `Fetching encrypted file chunk ${chunk.index + 1}/${chunks.length} (${chunk.dst_hash})`,
      );
      const downloaded = await downloadChunk(seedEndpoints, chunk.dst_hash, {
        onProgress,
      });
      return downloaded.content;
    },
  );

  onProgress(`Reconstructing ${file.name} with native ant-core WASM`);
  const content = decrypt(dataMap.content, encryptedChunks);
  if (!(content instanceof Uint8Array)) {
    throw new Error("ant-core WASM returned non-byte file content");
  }
  if (content.length !== file.size) {
    throw new Error(`Reconstructed file has ${content.length} bytes, expected ${file.size}`);
  }
  const hash = verifyChunk(file.blake3, content);
  onProgress(`Verified complete ${file.name} as ${hash}`);
  return { content, hash, dataMapNode: dataMap.node };
}
