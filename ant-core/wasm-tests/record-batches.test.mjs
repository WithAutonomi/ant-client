import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { encryptPublicFile } from "./pkg/ant_core.js";
import { mockWebRtc, paymentNetwork } from "./mock-webrtc.mjs";

const content = new TextEncoder().encode("Record batch regression fixture. ".repeat(2000));
const noSingle = () => { throw new Error("unexpected single payment"); };
const receipt = (_, quotes) => ({
  transactionHash: `0x${"ab".repeat(32)}`,
  totalAmount: quotes.reduce((sum, quote) => sum + BigInt(quote.amount), 0n).toString(),
});
const merkleReceipt = (_, request) => ({
  transactionHash: `0x${"cd".repeat(32)}`,
  winnerPoolHash: request.poolHashes[0],
  totalAmount: request.maximumAmount,
});

function encryptedRecords() {
  const encrypted = encryptPublicFile(content);
  const metadata = encrypted.records.map(record => ({ address: record.address, size: record.content.length }));
  return { encrypted, metadata };
}

test("consecutive record batches pay separately and download as one public file", async () => {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({})));
  const client = new BrowserNetworkClient(rtc.endpoints);
  const { encrypted, metadata } = encryptedRecords();
  const split = 2;
  const messages = [];
  let payments = 0;
  const pay = async (...args) => { payments++; return receipt(...args); };
  try {
    const first = await client.uploadRecords(
      { records: metadata.slice(0, split), first_index: 0, total_records: metadata.length }, paymentNetwork,
      index => encrypted.records[index].content, pay, message => messages.push(message));
    const second = await client.uploadRecords(
      { records: metadata.slice(split), first_index: split, total_records: metadata.length }, paymentNetwork,
      index => encrypted.records[split + index].content, pay, message => messages.push(message));
    assert.equal(payments, 2);
    assert.deepEqual([first.records, second.records], [split, metadata.length - split]);
    assert.deepEqual([first.paymentMode, second.paymentMode], ["single", "single"]);
    assert.equal(second.replicas, 4);
    const quoted = messages.map(message => /^Quoted record (\d+)\/(\d+)$/u.exec(message)).filter(Boolean);
    assert.deepEqual(new Set(quoted.map(match => Number(match[2]))), new Set([metadata.length]));
    assert.deepEqual(new Set(quoted.map(match => Number(match[1]))),
      new Set(metadata.map((_, index) => index + 1)));
    const downloaded = await client.downloadPublicFile(encrypted.address, 3);
    assert.deepEqual(downloaded.content, content);
  } finally { client.close(); }
});

test("record batches report the payment mode the shared coordinator used", async () => {
  const rtc = mockWebRtc(Array.from({ length: 16 }, () => ({})));
  const client = new BrowserNetworkClient(rtc.endpoints);
  const { encrypted, metadata } = encryptedRecords();
  try {
    const merkle = await client.uploadRecords({ records: metadata }, paymentNetwork,
      index => encrypted.records[index].content, noSingle, undefined, undefined, undefined, "merkle", merkleReceipt);
    assert.equal(merkle.paymentMode, "merkle");
    assert.equal(merkle.records, metadata.length);
    assert(rtc.requests.some(request => request.method === "merkle_quote"));
    const single = await client.uploadPublicFile(new TextEncoder().encode("single payment fixture"), "single.txt",
      "text/plain", paymentNetwork, receipt, undefined, undefined, undefined, "single");
    assert.equal(single.paymentMode, "single");
  } finally { client.close(); }
});

test("record batches reject empty and misplaced batches before any payment", async () => {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({})));
  const client = new BrowserNetworkClient(rtc.endpoints);
  const { encrypted, metadata } = encryptedRecords();
  let payments = 0;
  const pay = async (...args) => { payments++; return receipt(...args); };
  const load = index => encrypted.records[index].content;
  try {
    await assert.rejects(client.uploadRecords({ records: [] }, paymentNetwork, load, pay), /contains no records/);
    await assert.rejects(client.uploadRecords({ records: metadata, first_index: 1, total_records: metadata.length },
      paymentNetwork, load, pay), /extends past its file's record count/);
    await assert.rejects(client.uploadRecords({ records: [{ ...metadata[0], size: 0 }] }, paymentNetwork, load, pay),
      /invalid size 0/);
    assert.equal(payments, 0);
  } finally { client.close(); }
});
