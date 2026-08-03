import { hexToBytes } from "./protocol.js";

export const BROWSER_MANIFEST_VERSION = 2;
const MAX_PUBLIC_FILE_BYTES = 64 * 1024 * 1024;
const MAX_DATA_MAP_BYTES = 4 * 1024 * 1024;
const MAX_FILE_CHUNKS = 1024;

export function parseBrowserManifest(value) {
  if (!value || value.version !== BROWSER_MANIFEST_VERSION) {
    throw new Error(`Unsupported browser manifest version ${value?.version}`);
  }
  if (typeof value.network_id !== "string" || value.network_id.length === 0) {
    throw new Error("Browser manifest has no network ID");
  }
  if (!Array.isArray(value.endpoints) || value.endpoints.length === 0) {
    throw new Error("Browser manifest contains no WebTransport endpoints");
  }
  const endpoints = value.endpoints.map((endpoint) => {
    if (!endpoint || typeof endpoint.url !== "string") {
      throw new Error("Browser manifest endpoint has no URL");
    }
    if (!endpoint.url.startsWith("https://")) {
      throw new Error(`WebTransport endpoint must use HTTPS: ${endpoint.url}`);
    }
    hexToBytes(endpoint.peer_id ?? "", 32);
    hexToBytes(endpoint.certificate_sha256 ?? "", 32);
    return {
      peer_id: endpoint.peer_id.toLowerCase(),
      url: endpoint.url,
      certificate_sha256: endpoint.certificate_sha256.toLowerCase(),
    };
  });

  const files = (value.files ?? []).map((file) => {
    if (!file || typeof file.name !== "string" || file.name.length === 0) {
      throw new Error("Browser manifest file has no name");
    }
    hexToBytes(file.address ?? "", 32);
    if (
      !Number.isSafeInteger(file.size) ||
      file.size < 3 ||
      file.size > MAX_PUBLIC_FILE_BYTES
    ) {
      throw new Error(`Invalid public file size ${file.size}`);
    }
    hexToBytes(file.blake3 ?? "", 32);
    if (
      !Number.isSafeInteger(file.data_map_size) ||
      file.data_map_size < 1 ||
      file.data_map_size > MAX_DATA_MAP_BYTES
    ) {
      throw new Error(`Invalid DataMap size ${file.data_map_size}`);
    }
    if (
      !Array.isArray(file.chunks) ||
      file.chunks.length < 3 ||
      file.chunks.length > MAX_FILE_CHUNKS
    ) {
      throw new Error("Public file has an invalid self-encryption chunk list");
    }
    const chunks = file.chunks
      .map((chunk) => {
        if (!Number.isSafeInteger(chunk.index) || chunk.index < 0) {
          throw new Error(`Invalid file chunk index ${chunk.index}`);
        }
        hexToBytes(chunk.dst_hash ?? "", 32);
        hexToBytes(chunk.src_hash ?? "", 32);
        if (!Number.isSafeInteger(chunk.src_size) || chunk.src_size < 1) {
          throw new Error(`Invalid plaintext chunk size ${chunk.src_size}`);
        }
        return {
          index: chunk.index,
          dst_hash: chunk.dst_hash.toLowerCase(),
          src_hash: chunk.src_hash.toLowerCase(),
          src_size: chunk.src_size,
        };
      })
      .sort((left, right) => left.index - right.index);
    chunks.forEach((chunk, index) => {
      if (chunk.index !== index) {
        throw new Error("File chunk indices must be contiguous from zero");
      }
    });
    const reconstructedSize = chunks.reduce((total, chunk) => total + chunk.src_size, 0);
    if (reconstructedSize !== file.size) {
      throw new Error(
        `File chunk sizes total ${reconstructedSize}, expected ${file.size}`,
      );
    }
    return {
      name: file.name,
      address: file.address.toLowerCase(),
      size: file.size,
      content_type: file.content_type || "application/octet-stream",
      blake3: file.blake3.toLowerCase(),
      data_map_size: file.data_map_size,
      chunks,
      replicas: Number.isSafeInteger(file.replicas) ? file.replicas : 0,
    };
  });

  return {
    version: value.version,
    network_id: value.network_id,
    created_at: value.created_at,
    endpoints,
    files,
  };
}

export async function fetchBrowserManifest(url) {
  const response = await fetch(url, { cache: "no-store" });
  if (!response.ok) {
    throw new Error(`Manifest request failed with HTTP ${response.status}`);
  }
  return parseBrowserManifest(await response.json());
}
