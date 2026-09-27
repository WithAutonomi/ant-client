import init, { BrowserNetworkClient, encryptPublicFile } from "../wasm-tests/pkg/ant_core.js";
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
