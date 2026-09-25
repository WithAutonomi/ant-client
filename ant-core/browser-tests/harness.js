import init, { BrowserNodeClient, BrowserNetworkClient, encryptPublicFile, pointerAddress } from "../wasm-tests/pkg/ant_core.js";
import { Contract, JsonRpcProvider } from "ethers";
await init();
globalThis.runIntegration = async ({ endpoint, payment, rpcUrl }) => {
  const connector = new BrowserNodeClient(endpoint);
  if (connector.getChunk !== undefined) throw new Error("Unauthenticated connector exposes RPC methods");
  const session = await connector.connect();
  const hello = await session.hello();
  session.close();
  try { await session.getChunk("00".repeat(32)); throw new Error("Closed session accepted a request"); }
  catch (error) { if (!String(error).includes("session closed")) throw error; }
  session.free(); connector.free();
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
  let client = new BrowserNetworkClient([endpoint]);
  try {
    await client.uploadStagedPublicFile(staged, payment,
      index => { if (paid) throw new Error("injected post-payment storage interruption"); return encrypted.records[index].content; },
      pay, undefined, undefined, save, "single");
    throw new Error("Expected paid upload interruption");
  } catch (error) { if (!String(error).includes("injected post-payment")) throw error; }
  client.close(); client.free();
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
// Pointers (ADR-0016) through real nodes: each state paid on the local chain,
// stored on the close group, and read back by a client that wrote nothing.
globalThis.runPointerIntegration = async ({ endpoint, payment, rpcUrl }) => {
  const provider = new JsonRpcProvider(rpcUrl);
  const signer = await provider.getSigner(0);
  const token = new Contract(payment.payment_token_address, ["function approve(address,uint256) returns (bool)"], signer);
  const vault = new Contract(payment.payment_vault_address,
    ["function payForQuotes((address rewardsAddress,uint256 amount,bytes32 quoteHash)[] payments)"], signer);
  let payments = 0;
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
    return receipt;
  };
  const seed = crypto.getRandomValues(new Uint8Array(32));
  const inner = crypto.getRandomValues(new Uint8Array(32));
  const chunk = n => n.toString(16).padStart(2, "0").repeat(32);
  // A node admits 16 requests a second from one browser session (ADR-0015)
  // and closes the session past that. A paid pointer write asks each node about
  // eight things, and this wallet pays instantly on a local chain, so writes are
  // spaced as a real wallet's confirmation would space them.
  const paced = async write => { await new Promise(resolve => setTimeout(resolve, 1_500)); return write(); };
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
  provider.destroy();
  return { payments, address: created.pointer.address, expectedAddress: pointerAddress(seed),
    createdCounter: created.pointer.counter, updatedCounter: updated.pointer.counter,
    readCounter: read?.counter, readKind: read?.kind, readTarget: read?.target, endAddress: end.pointer.address,
    resolved, absent };
};
