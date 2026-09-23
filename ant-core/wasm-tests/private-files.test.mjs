import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { encryptPublicFile } from "./pkg/ant_core.js";
import { mockWebRtc, paymentNetwork } from "./mock-webrtc.mjs";

const content = new TextEncoder().encode("Private upload regression fixture. ".repeat(2000));
const maxRecordBytes = 4 * 1024 * 1024;
const receipt = (_, quotes) => ({
  transactionHash: `0x${"ab".repeat(32)}`,
  totalAmount: quotes.reduce((sum, quote) => sum + BigInt(quote.amount), 0n).toString(),
});

// The canonical DataMap record doubles as the native `.datamap` file content.
function privateRecords(original) {
  const encrypted = encryptPublicFile(original);
  return { encrypted, records: encrypted.records.slice(0, -1), dataMap: encrypted.records.at(-1).content };
}

test("a private upload keeps its DataMap local and downloads from it", async () => {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({})));
  const client = new BrowserNetworkClient(rtc.endpoints);
  const { encrypted, records, dataMap } = privateRecords(content);
  try {
    const uploaded = await client.uploadRecords(
      { records: records.map(record => ({ address: record.address, size: record.content.length })) },
      paymentNetwork, index => records[index].content, receipt);
    assert.equal(uploaded.records, records.length);
    assert(rtc.stores.every(store => !store.has(encrypted.address)), "the private DataMap must never be stored");
    const downloaded = await client.downloadPrivateFile(
      { data_map: dataMap, name: "private.txt", content_type: "text/plain" }, 3);
    assert.deepEqual(downloaded.content, content);
    assert.equal(downloaded.file.name, "private.txt");
    assert.equal(downloaded.file.size, content.length);
    assert.equal(downloaded.file.data_map_size, dataMap.length);
    assert.equal(downloaded.dataMapNode, undefined);
  } finally { client.close(); }
});

test("a private DataMap with nested records opens for range reads", async () => {
  const maxChunkSize = 4_190_208;
  const original = Uint8Array.from({ length: 3 * maxChunkSize + 1 }, (_, index) => index * 31);
  const { records, dataMap } = privateRecords(original);
  const rtc = mockWebRtc([{}]);
  for (const record of records) rtc.stores[0].set(record.address, record.content);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPrivateFile({ data_map: dataMap });
    assert.equal(reader.size, original.length);
    assert.match(reader.name, /^private-file-[0-9a-f]{16}\.bin$/u);
    const boundary = maxChunkSize;
    assert.deepEqual(await reader.readRange(boundary - 5, 10), original.slice(boundary - 5, boundary + 5));
    const downloaded = await client.downloadPrivateFile({ data_map: dataMap }, 3);
    assert.deepEqual(downloaded.content, original);
  } finally { reader?.close(); client.close(); }
});

test("oversized and corrupt private DataMaps fail before fetching records", async () => {
  const rtc = mockWebRtc([{}]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    await assert.rejects(client.downloadPrivateFile({ data_map: new Uint8Array(maxRecordBytes + 1) }, 3),
      /larger than any DataMap record/);
    await assert.rejects(client.openPrivateFile({ data_map: new Uint8Array([0xc1, 0x00, 0xff]) }),
      /Failed to deserialize DataMap/);
    assert.equal(rtc.requests.filter(request => request.method === "get_chunk").length, 0);
  } finally { client.close(); }
});
