import { deleteStagedRecords, getStagedRecord } from "./upload-record-store.js";

function uploadSessionId() {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

async function ensureUploadStorage(fileSize) {
  if (!navigator.storage?.estimate) return;
  try {
    await navigator.storage.persist?.();
  } catch {
    // Persistence is an eviction hint; IndexedDB remains usable when denied.
  }
  const { quota, usage = 0 } = await navigator.storage.estimate();
  if (quota === undefined) return;
  // Self-encryption chunks have compression-growth headroom. Keep an extra
  // 5% plus 16 MiB for DataMaps and the browser's storage bookkeeping.
  const required = Math.ceil(fileSize * 1.05) + 16 * 1024 * 1024;
  const available = Math.max(0, quota - usage);
  if (available < required) {
    throw new Error(
      `Not enough browser storage to stage this upload: ${available.toLocaleString()} bytes available, approximately ${required.toLocaleString()} required`,
    );
  }
}

export async function stageFileForUpload(file, onProgress = () => {}) {
  if (typeof Worker !== "function" || typeof indexedDB !== "object") {
    throw new Error("Large uploads require Web Workers and IndexedDB in this browser");
  }
  await ensureUploadStorage(file.size);
  const sessionId = uploadSessionId();
  const worker = new Worker(new URL("./upload-encryption-worker.js", import.meta.url), {
    type: "module",
  });
  return new Promise((resolve, reject) => {
    const finish = (callback, value) => {
      worker.terminate();
      callback(value);
    };
    worker.addEventListener("message", (event) => {
      if (event.data?.type === "progress") {
        onProgress(event.data.message);
      } else if (event.data?.type === "complete") {
        finish(resolve, { sessionId, staged: event.data.staged });
      } else if (event.data?.type === "error") {
        finish(reject, new Error(event.data.message));
      }
    });
    worker.addEventListener("error", (event) => {
      finish(reject, new Error(event.message || "Upload encryption worker failed"));
    });
    worker.postMessage({ type: "stage-file", file, sessionId });
  });
}

export async function loadStagedRecord(sessionId, index, _address, expectedSize) {
  const content = await getStagedRecord(sessionId, index);
  if (content.byteLength !== expectedSize) {
    throw new Error(
      `Staged upload record ${index + 1} has ${content.byteLength} bytes, expected ${expectedSize}`,
    );
  }
  return content;
}

export async function clearStagedUpload(sessionId, recordCount) {
  await deleteStagedRecords(sessionId, recordCount);
}
