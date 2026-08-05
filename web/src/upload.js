import { encryptPublicFile as encryptPublicFileNative } from "../pkg/ant_core.js";
import { BrowserNodeClient, iterativeFindClosest } from "./protocol.js";
import { payForStorageQuotes, verifyStorageQuote } from "./payment.js";

export const MAX_BROWSER_UPLOAD_BYTES = 64 * 1024 * 1024;
const MAX_STORE_TARGETS = 7;

export async function encryptPublicFile(
  content,
  {
    name = "upload.bin",
    contentType = "application/octet-stream",
    encrypt = encryptPublicFileNative,
  } = {},
) {
  if (!(content instanceof Uint8Array)) throw new Error("Upload content must be bytes");
  if (content.length < 3) throw new Error("Self-encryption requires a file of at least 3 bytes");
  if (content.length > MAX_BROWSER_UPLOAD_BYTES) {
    throw new Error(`Browser uploads are limited to ${MAX_BROWSER_UPLOAD_BYTES} bytes`);
  }
  const encrypted = encrypt(content);
  return {
    descriptor: {
      name,
      address: encrypted.address,
      size: content.length,
      content_type: contentType || "application/octet-stream",
      blake3: encrypted.blake3,
      data_map_size: encrypted.data_map_size,
      chunks: encrypted.chunks,
      replicas: 0,
    },
    records: encrypted.records,
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
  { onProgress = () => {}, encrypt = encryptPublicFileNative } = {},
) {
  const content = new Uint8Array(await file.arrayBuffer());
  onProgress(`Self-encrypting ${file.name} with native ant-core WASM (${content.length.toLocaleString()} bytes)`);
  const encrypted = await encryptPublicFile(content, {
    name: file.name,
    contentType: file.type,
    encrypt,
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
