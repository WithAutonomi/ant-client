import initAntCore, { BrowserFileEncryptor } from "../pkg/ant_core.js";
import { deleteStagedRecords, putStagedRecord } from "./upload-record-store.js";

const antCoreReady = initAntCore();

self.addEventListener("message", async (event) => {
  if (event.data?.type !== "stage-file") return;
  const { file, sessionId } = event.data;
  let storedRecords = 0;
  let encryptor;
  try {
    await antCoreReady;
    if (typeof FileReaderSync !== "function") {
      throw new Error("This browser cannot read selected files inside an upload worker");
    }
    const reader = new FileReaderSync();
    encryptor = new BrowserFileEncryptor(file.size, (offset, length) =>
      new Uint8Array(reader.readAsArrayBuffer(file.slice(offset, offset + length))),
    );
    self.postMessage({
      type: "progress",
      message: `Self-encrypting ${file.name} without loading it into page memory`,
    });

    while (true) {
      const record = encryptor.nextRecord();
      if (record === undefined) break;
      // IndexedDB takes ownership of a stable JS buffer before WASM advances.
      await putStagedRecord(sessionId, storedRecords, record.content.slice());
      storedRecords += 1;
      self.postMessage({
        type: "progress",
        message: `Encrypted and staged record ${storedRecords}`,
      });
    }
    const staged = encryptor.finish(file.name, file.type);
    if (staged.records.length !== storedRecords) {
      throw new Error(
        `Encryption produced ${staged.records.length} records but staged ${storedRecords}`,
      );
    }
    self.postMessage({ type: "complete", staged });
  } catch (error) {
    try {
      await deleteStagedRecords(sessionId, storedRecords);
    } catch {
      // Preserve the original encryption or storage error.
    }
    self.postMessage({
      type: "error",
      message: error instanceof Error ? error.message : String(error),
    });
  } finally {
    encryptor?.free();
  }
});
