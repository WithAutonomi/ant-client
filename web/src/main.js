import "./style.css";
import {
  BrowserNetworkClient,
  BrowserNodeClient,
  default as initAntCore,
} from "../pkg/ant_core.js";
import { fetchBrowserManifest } from "./manifest.js";
import { payForStorageQuotes } from "./payment.js";
import {
  clearStagedUpload,
  loadStagedRecord,
  stageFileForUpload,
} from "./upload-staging.js";

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
  streamFile: document.querySelector("#stream-file"),
  streamState: document.querySelector("#stream-state"),
  streamVideo: document.querySelector("#stream-video"),
  log: document.querySelector("#log"),
};

const pageParameters = new URLSearchParams(window.location.search);
const manifestOverride = pageParameters.get("manifest");
if (manifestOverride) elements.manifestUrl.value = manifestOverride;
const endpointOverride = pageParameters.get("endpoint");
if (endpointOverride) elements.endpointMultiaddr.value = endpointOverride;

let client;
let networkClient;
let browserManifest;
let downloadObjectUrl;
const videoReaders = new Map();
let activeVideoSession;
let protocolLog = "";
const MAX_PROTOCOL_LOG_CHARACTERS = 256 * 1024;

function timestamp() {
  return new Date().toLocaleTimeString();
}

function log(message, value) {
  const suffix =
    value === undefined ? "" : `\n${JSON.stringify(value, null, 2)}`;
  protocolLog += `[${timestamp()}] ${message}${suffix}\n`;
  if (protocolLog.length > MAX_PROTOCOL_LOG_CHARACTERS) {
    protocolLog = `[Earlier log entries removed]\n${protocolLog.slice(
      -MAX_PROTOCOL_LOG_CHARACTERS,
    )}`;
  }
  elements.log.textContent = protocolLog;
  elements.log.scrollTop = elements.log.scrollHeight;
}

function errorMessage(error) {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  if (error === undefined) return "unknown error";
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

function endpointFromForm() {
  return elements.endpointMultiaddr.value.trim();
}

function useEndpointAsBootstrap(multiaddr, hello) {
  const endpointWasInManifest = browserManifest?.endpoints.some(
    (endpoint) => endpoint.multiaddr === multiaddr,
  );
  const files = endpointWasInManifest ? browserManifest.files : [];
  if (!endpointWasInManifest) {
    stopVideoStream();
    elements.fileAddress.value = "";
    elements.publicFile.hidden = true;
  }
  networkClient?.close();
  networkClient = new BrowserNetworkClient([{ multiaddr }]);
  browserManifest = {
    version: browserManifest?.version ?? 5,
    network_id: endpointWasInManifest
      ? browserManifest.network_id
      : `manual-seed-${hello.peer_id}`,
    created_at: endpointWasInManifest
      ? browserManifest.created_at
      : new Date().toISOString(),
    endpoints: [{ multiaddr }],
    payment: hello.payment,
    files,
  };
  elements.manifestState.textContent = "Manual bootstrap · 1 direct node";
  elements.manifestState.classList.add("connected");
  log(`Using ${hello.peer_id} as the Rust network bootstrap seed`);
}

async function ensureVideoStreamWorker() {
  if (!("serviceWorker" in navigator)) {
    throw new Error("This browser does not support service workers");
  }
  await navigator.serviceWorker.register("/video-stream-sw.js", { scope: "/" });
  await navigator.serviceWorker.ready;
  if (navigator.serviceWorker.controller) return;
  await new Promise((resolve, reject) => {
    const timeout = setTimeout(
      () => reject(new Error("The video streaming service worker did not take control")),
      10_000,
    );
    navigator.serviceWorker.addEventListener(
      "controllerchange",
      () => {
        clearTimeout(timeout);
        resolve();
      },
      { once: true },
    );
  });
}

function stopVideoStream() {
  elements.streamVideo.pause();
  elements.streamVideo.removeAttribute("src");
  elements.streamVideo.load();
  elements.streamVideo.hidden = true;
  if (!activeVideoSession) return;
  videoReaders.get(activeVideoSession)?.close();
  videoReaders.delete(activeVideoSession);
  activeVideoSession = undefined;
}

function streamingUrl(sessionId, file) {
  const url = new URL(`/__autonomi_stream/${sessionId}/video`, location.origin);
  url.searchParams.set("size", String(file.size));
  url.searchParams.set("type", file.content_type || "application/octet-stream");
  url.searchParams.set("name", file.name);
  return url.href;
}

navigator.serviceWorker?.addEventListener("message", async (event) => {
  if (event.data?.type !== "autonomi-video-range") return;
  const port = event.ports[0];
  if (!port) return;
  try {
    const { sessionId, start, length } = event.data;
    const reader = videoReaders.get(sessionId);
    if (!reader) throw new Error("The requested video stream is no longer open");
    if (
      !Number.isSafeInteger(start) ||
      !Number.isSafeInteger(length) ||
      start < 0 ||
      length < 0
    ) {
      throw new Error("The service worker requested an invalid video range");
    }
    const bytes = await reader.readRange(start, length);
    const owned = bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength
      ? bytes
      : bytes.slice();
    port.postMessage({ ok: true, bytes: owned.buffer }, [owned.buffer]);
  } catch (error) {
    port.postMessage({ ok: false, error: errorMessage(error) });
  }
});

async function loadManifest() {
  elements.manifestState.classList.remove("connected");
  elements.manifestState.textContent = "Loading…";
  const manifest = await fetchBrowserManifest(
    elements.manifestUrl.value.trim(),
  );
  browserManifest = manifest;
  stopVideoStream();
  networkClient?.close();
  networkClient = new BrowserNetworkClient(manifest.endpoints);

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
  const multiaddr = endpointFromForm();
  const next = new BrowserNodeClient(multiaddr);
  const hello = await next.hello();
  client = next;
  useEndpointAsBootstrap(multiaddr, hello);
  elements.connectionState.textContent = `Connected · ${hello.peer_id.slice(0, 16)}…`;
  elements.connectionState.classList.add("connected");
  log("HELLO", hello);
  return next;
}

function reportError(context, error) {
  elements.connectionState.textContent = `${context} failed`;
  log(`${context} failed: ${errorMessage(error)}`);
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
    log(`Manifest load failed: ${errorMessage(error)}`);
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
    if (!networkClient) throw new Error("Load the browser testnet manifest first");
    const result = await networkClient.findClosest(target, (message) => log(message));
    log("Closest nodes", result);
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
  let stagedUpload;
  try {
    if (!browserManifest)
      throw new Error("Load the browser testnet manifest first");
    const file = elements.uploadInput.files?.[0];
    if (!file) throw new Error("Choose a file to upload");
    if (!walletSecret) throw new Error("Enter the paying wallet secret key");

    log(`Starting paid public upload for ${file.name}`);
    if (!networkClient) throw new Error("Browser network client is not ready");
    const onProgress = (message) => {
      elements.uploadState.textContent = message;
      log(message);
    };
    stagedUpload = await stageFileForUpload(file, onProgress);
    const result = await networkClient.uploadStagedPublicFile(
      stagedUpload.staged,
      browserManifest.payment,
      (index, address, size) =>
        loadStagedRecord(stagedUpload.sessionId, index, address, size),
      (paymentNetwork, quotes) =>
        payForStorageQuotes(paymentNetwork, quotes, walletSecret, { onProgress }),
      onProgress,
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
    log(`File upload failed: ${errorMessage(error)}`);
    console.error(error);
  } finally {
    walletSecret = "";
    if (stagedUpload) {
      try {
        await clearStagedUpload(
          stagedUpload.sessionId,
          stagedUpload.staged.records.length,
        );
      } catch (error) {
        log(`Could not clear temporary upload records: ${errorMessage(error)}`);
      }
    }
    elements.uploadFile.disabled = false;
  }
});

elements.downloadFile.addEventListener("click", async () => {
  elements.downloadState.classList.remove("connected");
  elements.downloadState.textContent = "Preparing save…";
  elements.downloadFile.disabled = true;
  try {
    const address = bytesToHex(hexToBytes(elements.fileAddress.value, 32));
    const published = browserManifest?.files.find(
      (file) => file.address === address,
    );
    elements.downloadState.textContent = "Downloading…";
    log(published
      ? `Downloading complete public file ${published.name} (${published.size.toLocaleString()} bytes)`
      : `Resolving and downloading public file ${address} directly from its DataMap`);
    if (!networkClient) throw new Error("Browser network client is not ready");
    const { content, hash, file, dataMapNode } = await networkClient.downloadPublicFile(
      published ?? address,
      3,
      (message) => log(message),
    );
    const saveHandle = await chooseSaveHandle(file.name);
    const savedDirectly = await exposeSavedFile(file, content, saveHandle);
    elements.downloadState.textContent = `${
      savedDirectly ? "Saved" : "Browser download started"
    } · ${content.length.toLocaleString()} bytes`;
    elements.downloadState.classList.add("connected");
    log(
      `${
        savedDirectly ? "Saved" : "Started browser save for"
      } whole-file BLAKE3-verified ${file.name} from direct nodes as ${hash}`,
      { data_map_node: dataMapNode.peer_id, chunks: file.chunks.length },
    );
  } catch (error) {
    elements.downloadState.textContent =
      error.name === "AbortError" ? "Save cancelled" : "Failed";
    log(`File download failed: ${errorMessage(error)}`);
    console.error(error);
  } finally {
    elements.downloadFile.disabled = false;
  }
});

elements.streamFile.addEventListener("click", async () => {
  elements.streamState.classList.remove("connected");
  elements.streamState.textContent = "Opening stream…";
  elements.streamFile.disabled = true;
  try {
    const address = bytesToHex(hexToBytes(elements.fileAddress.value, 32));
    const published = browserManifest?.files.find(
      (file) => file.address === address,
    );
    if (!networkClient) throw new Error("Browser network client is not ready");
    stopVideoStream();
    await ensureVideoStreamWorker();
    const reader = await networkClient.openPublicFile(published ?? address, (message) => {
      elements.streamState.textContent = message;
      log(message);
    });
    const resolved = published ?? {
      address,
      name: reader.name,
      size: reader.size,
      content_type: reader.contentType,
    };
    const sessionId = bytesToHex(crypto.getRandomValues(new Uint8Array(16)));
    videoReaders.set(sessionId, reader);
    activeVideoSession = sessionId;
    elements.streamVideo.src = streamingUrl(sessionId, resolved);
    elements.streamVideo.hidden = false;
    elements.streamState.textContent = "Ready · press play to stream";
    elements.streamState.classList.add("connected");
    log(`Prepared random-access video stream for ${resolved.name}`, {
      size: resolved.size,
      content_type: resolved.content_type,
      session_id: sessionId,
    });
  } catch (error) {
    stopVideoStream();
    elements.streamState.textContent = "Stream failed";
    log(`Video stream failed: ${errorMessage(error)}`);
    console.error(error);
  } finally {
    elements.streamFile.disabled = false;
  }
});

elements.streamVideo.addEventListener("error", () => {
  const mediaError = elements.streamVideo.error;
  if (!mediaError || !activeVideoSession) return;
  elements.streamState.textContent = `Playback error (${mediaError.code})`;
  log(`Video element could not decode the selected file (media error ${mediaError.code})`);
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
  stopVideoStream();
  client?.close();
  networkClient?.close();
});

function bytesToHex(bytes) {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function hexToBytes(value, expectedLength) {
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

elements.randomTarget.click();
if (endpointOverride) {
  elements.manifestState.textContent = "Manual endpoint · manifest skipped";
  log("Ready. Using the WebRTC Direct endpoint from the page URL.");
} else {
  log("Ready. Loading the local browser testnet manifest…");
  loadManifest().catch((error) => {
    elements.manifestState.textContent = "Not running";
    log(`Local manifest not available yet: ${errorMessage(error)}`);
  });
}
