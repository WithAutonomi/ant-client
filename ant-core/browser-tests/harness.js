import init, {
  BrowserNetworkClient, createFileLink, createManifest, decodeManifest, encryptPublicFile, parseManifestLink,
  pointerAddress, recoverPointerPayment,
} from "../wasm-tests/pkg/ant_core.js";
import { Contract, JsonRpcProvider } from "ethers";
await init();
globalThis.runIntegration = async ({ endpoint, payment, rpcUrl }) => {
  let client = new BrowserNetworkClient([endpoint]);
  const hello = await client.connect(payment);
  const content = new TextEncoder().encode("Real browser paid recovery and canonical DataMap. ".repeat(256));
  const encrypted = encryptPublicFile(content);
  const staged = { name: "integration.txt", content_type: "text/plain", address: encrypted.address,
    records: encrypted.records.map(record => ({ address: record.address, size: record.content.length })) };
  const provider = new JsonRpcProvider(rpcUrl);
  const signer = await provider.getSigner(0);
  const token = new Contract(payment.payment_token_address, ["function approve(address,uint256) returns (bool)"], signer);
  const vault = new Contract(payment.payment_vault_address,
    ["function payForQuotes((address rewardsAddress,uint256 amount,bytes32 quoteHash)[] payments)"], signer);
  let payments = 0, checkpoint, paid = false;
  const save = value => { checkpoint = value; localStorage.setItem("checkpoint", value); };
  const pay = async (_, quotes, submitted) => {
    payments++;
    const totalAmount = quotes.reduce((sum, quote) => sum + BigInt(quote.amount), 0n);
    await (await token.approve(payment.payment_vault_address, totalAmount)).wait();
    const transaction = await vault.payForQuotes(quotes.map(quote => ({
      rewardsAddress: quote.rewardsAddress, amount: quote.amount, quoteHash: `0x${quote.quoteHash.replace(/^0x/, "")}`,
    })));
    const receipt = { transactionHash: transaction.hash, totalAmount: totalAmount.toString() };
    await submitted(receipt);
    await transaction.wait();
    paid = true;
    return receipt;
  };
  try {
    await client.uploadStagedPublicFile(staged, payment,
      index => { if (paid) throw new Error("injected post-payment storage interruption"); return encrypted.records[index].content; },
      pay, undefined, undefined, save, "single");
    throw new Error("Expected paid upload interruption");
  } catch (error) { if (!String(error).includes("injected post-payment")) throw error; }
  client.close();
  try { await client.connect(payment); throw new Error("Closed network client accepted a connection"); }
  catch (error) { if (!String(error).includes("pool is closed")) throw error; }
  client.free();
  client = new BrowserNetworkClient([endpoint]);
  const result = await client.uploadStagedPublicFile(staged, payment, index => encrypted.records[index].content,
    () => { throw new Error("Recovery tried to pay again"); }, undefined, checkpoint, save, "single");
  client.close(); client.free();
  const reader = new BrowserNetworkClient([endpoint]);
  const downloaded = await reader.downloadPublicFile(result.file.address, 4);
  reader.close(); reader.free();
  provider.destroy();
  return { payments, size: result.file.size, expectedSize: content.length,
    matches: downloaded.content.length === content.length && downloaded.content.every((value, index) => value === content[index]),
    hello: hello.type, records: result.records, replicas: result.file.replicas };
};
// A wallet paying on the local chain, reporting each broadcast and counting
// the payments it is asked for.
const chainWallet = async (payment, rpcUrl) => {
  const provider = new JsonRpcProvider(rpcUrl);
  const signer = await provider.getSigner(0);
  const token = new Contract(payment.payment_token_address, ["function approve(address,uint256) returns (bool)"], signer);
  const vault = new Contract(payment.payment_vault_address,
    ["function payForQuotes((address rewardsAddress,uint256 amount,bytes32 quoteHash)[] payments)"], signer);
  const wallet = { payments: 0, destroy: () => provider.destroy() };
  wallet.pay = async (_, quotes, submitted) => {
    wallet.payments++;
    const totalAmount = quotes.reduce((sum, quote) => sum + BigInt(quote.amount), 0n);
    await (await token.approve(payment.payment_vault_address, totalAmount)).wait();
    const transaction = await vault.payForQuotes(quotes.map(quote => ({
      rewardsAddress: quote.rewardsAddress, amount: quote.amount, quoteHash: `0x${quote.quoteHash.replace(/^0x/, "")}`,
    })));
    const receipt = { transactionHash: transaction.hash, totalAmount: totalAmount.toString() };
    await submitted(receipt);
    await transaction.wait();
    return receipt;
  };
  return wallet;
};
const chunk = n => n.toString(16).padStart(2, "0").repeat(32);
// A node admits 16 requests a second from one browser session (ADR-0015)
// and closes the session past that. A paid pointer write asks each node about
// eight things, and this wallet pays instantly on a local chain, so writes are
// spaced as a real wallet's confirmation would space them.
const paced = async write => { await new Promise(resolve => setTimeout(resolve, 1_500)); return write(); };
// Pointers (ADR-0016) through real nodes: each state paid on the local chain,
// stored on the close group, and read back by a client that wrote nothing.
globalThis.runPointerIntegration = async ({ endpoint, payment, rpcUrl }) => {
  const wallet = await chainWallet(payment, rpcUrl);
  const pay = wallet.pay;
  const seed = crypto.getRandomValues(new Uint8Array(32));
  const inner = crypto.getRandomValues(new Uint8Array(32));
  const writer = new BrowserNetworkClient([endpoint]);
  const created = await writer.createPointer(seed, chunk(1), "chunk", payment, pay);
  const updated = await paced(() => writer.updatePointer(seed, chunk(2), "chunk", payment, pay));
  const end = await paced(() => writer.createPointer(inner, chunk(3), "chunk", payment, pay));
  await paced(() => writer.updatePointer(seed, end.pointer.address, "pointer", payment, pay));
  writer.close(); writer.free();
  const reader = new BrowserNetworkClient([endpoint]);
  const read = await reader.getPointer(created.pointer.address);
  const resolved = await reader.resolvePointer(created.pointer.address);
  const absent = await reader.getPointer(chunk(0x42));
  reader.close(); reader.free();
  // Hand the pointer over for good to the one `inner` controls (ADR-0018 in
  // ant-node), then ask the whole group whether that is final.
  const handover = new BrowserNetworkClient([endpoint]);
  const transferred = await paced(() => handover.transferPointer(seed, end.pointer.address, payment, pay));
  handover.close(); handover.free();
  const checker = new BrowserNetworkClient([endpoint]);
  const finality = await checker.pointerFinality(created.pointer.address);
  checker.close(); checker.free();
  wallet.destroy();
  return { payments: wallet.payments, address: created.pointer.address, expectedAddress: pointerAddress(seed),
    createdCounter: created.pointer.counter, updatedCounter: updated.pointer.counter,
    readCounter: read?.counter, readKind: read?.kind, readTarget: read?.target, endAddress: end.pointer.address,
    resolved, absent, transferredCounter: transferred.pointer.counter,
    finalityStatus: finality.status, finalityTransferredTo: finality.majority?.transferredTo };
};
// A page lost once its wallet's payment confirmed, before the write stored the
// state: rebuilt from the checkpoint and the broadcast the page kept, stored
// without paying again. Then a manifest built in the page.
globalThis.runPointerRecoveryIntegration = async ({ endpoint, payment, rpcUrl }) => {
  const wallet = await chainWallet(payment, rpcUrl);
  const seed = crypto.getRandomValues(new Uint8Array(32));
  const address = pointerAddress(seed);
  let checkpoint, submission, lostError;
  const lost = async (...args) => {
    await wallet.pay(...args);
    throw new Error("page lost after the payment confirmed");
  };
  const writer = new BrowserNetworkClient([endpoint]);
  try {
    await writer.createPointer(seed, chunk(5), "chunk", payment, lost, {
      onPrepared: value => { checkpoint = value; },
      onSubmission: value => { submission = value; },
    });
  } catch (error) { lostError = String(error); }
  writer.close(); writer.free();
  const recovered = recoverPointerPayment(checkpoint, submission);
  const storer = new BrowserNetworkClient([endpoint]);
  const beforeRecovery = await storer.getPointer(address);
  const stored = await paced(() => storer.storePaidPointer(recovered.record, recovered.proof, payment));
  let existsError;
  try { await paced(() => storer.createPointer(seed, chunk(6), "chunk", payment, wallet.pay)); }
  catch (error) { existsError = String(error); }
  storer.close(); storer.free();
  const reader = new BrowserNetworkClient([endpoint]);
  const read = await reader.getPointer(address);
  reader.close(); reader.free();
  wallet.destroy();
  const file = encryptPublicFile(new TextEncoder().encode("a manifest entry"));
  const created = createManifest({
    name: "recovered", torrent: { infoHashV1: "ab".repeat(20) },
    entries: [{ kind: "public", path: "docs/readme.txt", size: 16, address: createFileLink(file.address) }],
  });
  const decoded = decodeManifest(created.bytes);
  return { payments: wallet.payments, lostError, submission, beforeRecovery, stored, read, existsError,
    expectedAddress: address, fileAddress: file.address, manifestEntry: decoded.entries[0],
    manifestBytes: Array.from(created.bytes), link: created.link,
    linkMatches: JSON.stringify(parseManifestLink(created.link).manifest) === JSON.stringify(decoded) };
};
// The swarm: pages reading the same file share its records over WebRTC, met
// through the nodes closest to it. A page's client stays open after a read,
// in the file's room, to serve the pages that read it next.
const sha256 = async bytes => Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
  byte => byte.toString(16).padStart(2, "0")).join("");
globalThis.runSwarmUpload = async ({ endpoint, payment, rpcUrl, size }) => {
  const wallet = await chainWallet(payment, rpcUrl);
  const content = new Uint8Array(size);
  // Incompressible, so the records are full size; at most 64 KiB per call.
  for (let at = 0; at < size; at += 65_536) crypto.getRandomValues(content.subarray(at, at + 65_536));
  const encrypted = encryptPublicFile(content);
  const staged = { name: "swarm.bin", content_type: "application/octet-stream", address: encrypted.address,
    records: encrypted.records.map(record => ({ address: record.address, size: record.content.length })) };
  const uploader = new BrowserNetworkClient([endpoint]);
  const result = await uploader.uploadStagedPublicFile(staged, payment, index => encrypted.records[index].content,
    wallet.pay, undefined, undefined, () => {}, "single");
  uploader.close(); uploader.free(); wallet.destroy();
  return { address: result.file.address, records: result.records, sha: await sha256(content) };
};
globalThis.runSwarmRead = async ({ endpoint, address, meet }) => {
  const client = new BrowserNetworkClient([endpoint]);
  const hello = await client.connect();
  await client.joinSwarm(address);
  if (meet) {
    const deadline = Date.now() + 60_000;
    while (client.swarmStats().links === 0) {
      if (Date.now() > deadline) throw new Error(`never met another reader: ${JSON.stringify(client.swarmStats())}`);
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    // The first messages on a new link say what each side holds.
    await new Promise(resolve => setTimeout(resolve, 500));
  }
  const downloaded = await client.downloadPublicFile(address, 4);
  globalThis.swarmClient = client;
  return { capabilities: hello.capabilities ?? [], sha: await sha256(downloaded.content), stats: client.swarmStats() };
};
globalThis.swarmStatsNow = () => globalThis.swarmClient.swarmStats();
