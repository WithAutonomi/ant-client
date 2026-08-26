import "./style.css";
import initAntCore from "../pkg/ant_core.js";
import {
  BrowserNodeClient,
  bytesToHex,
  hexToBytes,
  iterativeFindClosest,
} from "./protocol.js";
import { downloadPublicFile } from "./file.js";
import { fetchBrowserManifest } from "./manifest.js";
import { uploadPublicFile } from "./upload.js";

await initAntCore();

const elements = {
  manifestUrl: document.querySelector("#manifest-url"),
  loadManifest: document.querySelector("#load-manifest"),
  manifestState: document.querySelector("#manifest-state"),
  publicFile: document.querySelector("#public-file"),
  publicFileName: document.querySelector("#public-file-name"),
  publicFileAddress: document.querySelector("#public-file-address"),
  publicFileChunks: document.querySelector("#public-file-chunks"),
  publicFileReplicas: document.querySelector("#public-file-replicas"),
  endpointMultiaddr: document.querySelector("#endpoint-multiaddr"),
  connect: document.querySelector("#connect"),
  connectionState: document.querySelector("#connection-state"),
  lookupTarget: document.querySelector("#lookup-target"),
  randomTarget: document.querySelector("#random-target"),
  findClosest: document.querySelector("#find-closest"),
  uploadInput: document.querySelector("#upload-file-input"),
  walletSecret: document.querySelector("#wallet-secret"),
  uploadFile: document.querySelector("#upload-file"),
  uploadState: document.querySelector("#upload-state"),
  uploadResult: document.querySelector("#upload-result"),
  uploadResultName: document.querySelector("#upload-result-name"),
  uploadResultAddress: document.querySelector("#upload-result-address"),
  uploadResultRecords: document.querySelector("#upload-result-records"),
  uploadResultPayment: document.querySelector("#upload-result-payment"),
  fileAddress: document.querySelector("#file-address"),
  downloadFile: document.querySelector("#download-file"),
  downloadState: document.querySelector("#download-state"),
  downloadLink: document.querySelector("#download-link"),
  log: document.querySelector("#log"),
};

let client;
let browserManifest;
let downloadObjectUrl;

function timestamp() {
  return new Date().toLocaleTimeString();
}

function log(message, value) {
  const suffix =
    value === undefined ? "" : `\n${JSON.stringify(value, null, 2)}`;
  elements.log.textContent += `[${timestamp()}] ${message}${suffix}\n`;
  elements.log.scrollTop = elements.log.scrollHeight;
}

function endpointFromForm() {
  return elements.endpointMultiaddr.value.trim();
}

function seedEndpoints() {
  return browserManifest?.endpoints?.length
    ? browserManifest.endpoints
    : [endpointFromForm()];
}

async function loadManifest() {
  elements.manifestState.classList.remove("connected");
  elements.manifestState.textContent = "Loading…";
  const manifest = await fetchBrowserManifest(
    elements.manifestUrl.value.trim(),
  );
  browserManifest = manifest;

  const first = manifest.endpoints[0];
  elements.endpointMultiaddr.value = first.multiaddr;
  client?.close();
  client = undefined;
  elements.connectionState.classList.remove("connected");
  elements.connectionState.textContent = "Disconnected";

  const file = manifest.files[0];
  if (file) {
    elements.fileAddress.value = file.address;
    elements.publicFileName.textContent = `${file.name} · ${file.size.toLocaleString()} bytes`;
    elements.publicFileAddress.textContent = file.address;
    elements.publicFileChunks.textContent = `${file.chunks.length} encrypted data chunks + public DataMap`;
    elements.publicFileReplicas.textContent = `${file.replicas} node${file.replicas === 1 ? "" : "s"}`;
    elements.publicFile.hidden = false;
  } else {
    elements.publicFile.hidden = true;
  }

  elements.manifestState.textContent = `${manifest.endpoints.length} direct nodes · ${manifest.files.length} file${manifest.files.length === 1 ? "" : "s"}`;
  elements.manifestState.classList.add("connected");
  log(`Loaded browser manifest ${manifest.network_id}`, manifest);
  return manifest;
}

async function connectedClient() {
  if (client) return client;
  const next = new BrowserNodeClient(endpointFromForm());
  const hello = await next.hello();
  client = next;
  elements.connectionState.textContent = `Connected · ${hello.peer_id.slice(0, 16)}…`;
  elements.connectionState.classList.add("connected");
  log("HELLO", hello);
  return next;
}

function reportError(context, error) {
  elements.connectionState.textContent = `${context} failed`;
  log(`${context} failed: ${error.message}`);
  console.error(error);
}

elements.randomTarget.addEventListener("click", () => {
  const target = crypto.getRandomValues(new Uint8Array(32));
  elements.lookupTarget.value = bytesToHex(target);
});

elements.loadManifest.addEventListener("click", async () => {
  try {
    await loadManifest();
  } catch (error) {
    elements.manifestState.textContent = "Load failed";
    log(`Manifest load failed: ${error.message}`);
    console.error(error);
  }
});

elements.connect.addEventListener("click", async () => {
  client?.close();
  client = undefined;
  elements.connectionState.classList.remove("connected");
  elements.connectionState.textContent = "Connecting…";
  try {
    await connectedClient();
  } catch (error) {
    reportError("Connection", error);
  }
});

elements.findClosest.addEventListener("click", async () => {
  try {
    const target = elements.lookupTarget.value.trim();
    hexToBytes(target, 32);
    log(`Starting iterative lookup for ${target}`);
    const result = await iterativeFindClosest(seedEndpoints(), target, {
      onProgress: (message) => log(message),
    });
    log("Closest nodes", {
      nodes: result.nodes,
      queried: result.queried,
      failures: result.failures.map(({ peerId, error }) => ({
        peerId,
        message: error.message,
      })),
    });
    if (result.ownsClientPool) result.clientPool.close();
  } catch (error) {
    reportError("Lookup", error);
  }
});

elements.uploadFile.addEventListener("click", async () => {
  elements.uploadState.classList.remove("connected");
  elements.uploadState.textContent = "Preparing upload…";
  elements.uploadResult.hidden = true;
  elements.uploadFile.disabled = true;
  let walletSecret = elements.walletSecret.value.trim();
  elements.walletSecret.value = "";
  try {
    if (!browserManifest)
      throw new Error("Load the browser testnet manifest first");
    const file = elements.uploadInput.files?.[0];
    if (!file) throw new Error("Choose a file to upload");
    if (!walletSecret) throw new Error("Enter the paying wallet secret key");

    log(`Starting paid public upload for ${file.name}`);
    const result = await uploadPublicFile(
      seedEndpoints(),
      browserManifest.payment,
      file,
      walletSecret,
      {
        onProgress: (message) => {
          elements.uploadState.textContent = message;
          log(message);
        },
      },
    );

    browserManifest.files = [
      ...browserManifest.files.filter(
        (published) => published.address !== result.file.address,
      ),
      result.file,
    ];
    elements.fileAddress.value = result.file.address;
    elements.uploadResultName.textContent = `${result.file.name} · ${result.file.size.toLocaleString()} bytes`;
    elements.uploadResultAddress.textContent = result.file.address;
    elements.uploadResultRecords.textContent = `${result.records} records · at least ${result.file.replicas} replica${result.file.replicas === 1 ? "" : "s"}`;
    elements.uploadResultPayment.textContent = result.transactionHash
      ? `${result.transactionHash} · ${result.storageCostAtto} atto tokens`
      : "No new payment was required";
    elements.uploadResult.hidden = false;
    elements.uploadState.textContent = "Uploaded · ready to download";
    elements.uploadState.classList.add("connected");
    log(
      `Uploaded and registered ${result.file.name} for immediate download`,
      result,
    );
  } catch (error) {
    elements.uploadState.textContent = "Upload failed";
    log(`File upload failed: ${error.message}`);
    console.error(error);
  } finally {
    walletSecret = "";
    elements.uploadFile.disabled = false;
  }
});

elements.downloadFile.addEventListener("click", async () => {
  elements.downloadState.classList.remove("connected");
  elements.downloadState.textContent = "Preparing save…";
  elements.downloadFile.disabled = true;
  try {
    const address = elements.fileAddress.value.trim().toLowerCase();
    hexToBytes(address, 32);
    const published = browserManifest?.files.find(
      (file) => file.address === address,
    );
    if (!published) {
      throw new Error(
        "That public file address is not described by the loaded testnet manifest",
      );
    }
    const saveHandle = await chooseSaveHandle(published.name);
    elements.downloadState.textContent = "Downloading…";
    log(
      `Downloading complete public file ${published.name} (${published.size.toLocaleString()} bytes)`,
    );
    const { content, hash, dataMapNode } = await downloadPublicFile(
      seedEndpoints(),
      published,
      {
        onProgress: (message) => log(message),
      },
    );
    const savedDirectly = await exposeSavedFile(published, content, saveHandle);
    elements.downloadState.textContent = `${
      savedDirectly ? "Saved" : "Browser download started"
    } · ${content.length.toLocaleString()} bytes`;
    elements.downloadState.classList.add("connected");
    log(
      `${
        savedDirectly ? "Saved" : "Started browser save for"
      } whole-file BLAKE3-verified ${published.name} from direct nodes as ${hash}`,
      { data_map_node: dataMapNode.peer_id, chunks: published.chunks.length },
    );
  } catch (error) {
    elements.downloadState.textContent =
      error.name === "AbortError" ? "Save cancelled" : "Failed";
    log(`File download failed: ${error.message}`);
    console.error(error);
  } finally {
    elements.downloadFile.disabled = false;
  }
});

async function chooseSaveHandle(name) {
  if (typeof window.showSaveFilePicker !== "function") return undefined;
  return window.showSaveFilePicker({ suggestedName: name });
}

async function exposeSavedFile(file, content, saveHandle) {
  if (saveHandle) {
    const writable = await saveHandle.createWritable();
    try {
      await writable.write(content);
      await writable.close();
    } catch (error) {
      try {
        await writable.abort(error);
      } catch (abortError) {
        log(`Could not abort failed save: ${abortError.message}`);
      }
      throw error;
    }
  }

  if (downloadObjectUrl) URL.revokeObjectURL(downloadObjectUrl);
  downloadObjectUrl = URL.createObjectURL(
    new Blob([content], {
      type: file.content_type ?? "application/octet-stream",
    }),
  );
  const anchor = document.createElement("a");
  anchor.href = downloadObjectUrl;
  anchor.download = file.name;
  anchor.textContent = saveHandle
    ? `Save ${file.name} again`
    : `Save ${file.name} (${content.length.toLocaleString()} bytes)`;
  elements.downloadLink.replaceChildren(anchor);
  if (!saveHandle) anchor.click();
  return Boolean(saveHandle);
}

window.addEventListener("beforeunload", () => {
  if (downloadObjectUrl) URL.revokeObjectURL(downloadObjectUrl);
});

elements.randomTarget.click();
log("Ready. Loading the local browser testnet manifest…");
loadManifest().catch((error) => {
  elements.manifestState.textContent = "Not running";
  log(`Local manifest not available yet: ${error.message}`);
});
