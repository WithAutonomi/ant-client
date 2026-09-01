const DATABASE_NAME = "autonomi-browser-upload-staging";
const DATABASE_VERSION = 1;
const RECORD_STORE = "records";

let databasePromise;

function uploadDatabase() {
  if (!databasePromise) {
    databasePromise = new Promise((resolve, reject) => {
      const request = indexedDB.open(DATABASE_NAME, DATABASE_VERSION);
      request.addEventListener("upgradeneeded", () => {
        if (!request.result.objectStoreNames.contains(RECORD_STORE)) {
          request.result.createObjectStore(RECORD_STORE);
        }
      });
      request.addEventListener("success", () => resolve(request.result));
      request.addEventListener("error", () =>
        reject(request.error ?? new Error("Could not open upload staging storage")),
      );
    });
  }
  return databasePromise;
}

function transactionDone(transaction) {
  return new Promise((resolve, reject) => {
    transaction.addEventListener("complete", resolve, { once: true });
    transaction.addEventListener(
      "abort",
      () => reject(transaction.error ?? new Error("Upload staging transaction aborted")),
      { once: true },
    );
    transaction.addEventListener(
      "error",
      () => reject(transaction.error ?? new Error("Upload staging transaction failed")),
      { once: true },
    );
  });
}

export async function putStagedRecord(sessionId, index, content) {
  const database = await uploadDatabase();
  const transaction = database.transaction(RECORD_STORE, "readwrite");
  transaction.objectStore(RECORD_STORE).put(content, [sessionId, index]);
  await transactionDone(transaction);
}

export async function getStagedRecord(sessionId, index) {
  const database = await uploadDatabase();
  const transaction = database.transaction(RECORD_STORE, "readonly");
  const request = transaction.objectStore(RECORD_STORE).get([sessionId, index]);
  const result = await new Promise((resolve, reject) => {
    request.addEventListener("success", () => resolve(request.result), { once: true });
    request.addEventListener(
      "error",
      () => reject(request.error ?? new Error("Could not read a staged upload record")),
      { once: true },
    );
  });
  await transactionDone(transaction);
  if (result === undefined) {
    throw new Error(`Staged upload record ${index + 1} is missing`);
  }
  return result instanceof Uint8Array ? result : new Uint8Array(result);
}

export async function deleteStagedRecords(sessionId, recordCount) {
  if (!recordCount) return;
  const database = await uploadDatabase();
  const transaction = database.transaction(RECORD_STORE, "readwrite");
  const store = transaction.objectStore(RECORD_STORE);
  for (let index = 0; index < recordCount; index += 1) {
    store.delete([sessionId, index]);
  }
  await transactionDone(transaction);
}
