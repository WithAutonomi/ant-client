import { BrowserNetworkClient } from "./client-fixture.mjs";
import assert from "node:assert/strict";
import test from "node:test";
import { contentAddress, encryptPublicFile, parseWebRtcDirectMultiaddr, test_encode_public_data_map } from "./pkg/ant_core.js";
import { mockWebRtc, paymentNetwork } from "./mock-webrtc.mjs";

const content = new TextEncoder().encode("Discoverable storage holder regression.".repeat(100));
const encrypted = encryptPublicFile(content);
const datamap = encrypted.records.at(-1);

function failDiscovery(channel, method) {
  if (method !== "find_node") return;
  // Fail only discovery, not authenticated GET. A new association can still
  // serve the file, as with a transient FIND_NODE timeout in a real network.
  channel.emit(new ArrayBuffer(0));
  return false;
}

test("a known holder remains readable when its latest discovery request fails", async () => {
  const rtc = mockWebRtc([{ chunk: datamap.content, respond: failDiscovery }, {}]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(datamap.address);
    assert.equal(reader.size, content.length);
    assert(rtc.requests.some(request => request.node === 0 && request.method === "get_chunk"));
  } finally { reader?.close(); client.close(); }
});

test("GET can recover even when no node answers discovery", async () => {
  const rtc = mockWebRtc([{ chunk: datamap.content, respond: failDiscovery }]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(datamap.address);
    assert.equal(reader.size, content.length);
  } finally { reader?.close(); client.close(); }
});

test("a connected known holder can finish a verified read while discovery is still pending", async () => {
  const options = [{ chunk: datamap.content }];
  const rtc = mockWebRtc(options);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader, deadline;
  try {
    await client.findClosest(datamap.address);
    // GET remains responsive while FIND_NODE cannot finish. The same shared
    // read policy is used by native, and a miss would still await discovery.
    options[0].respond = (_channel, method) => method === "find_node" ? false : undefined;
    reader = await Promise.race([
      client.openPublicFile(datamap.address),
      new Promise((_, reject) => deadline = setTimeout(() => reject(new Error("read waited for discovery")), 1000)),
    ]);
    assert.equal(reader.size, content.length);
    assert.equal(rtc.connections.length, 1);
    assert.equal(rtc.requests.filter(request => request.method === "get_chunk").length, 1);
  } finally { clearTimeout(deadline); reader?.close(); client.close(); }
});


test("a newly discovered holder is readable before a slow lookup round finishes", async () => {
  const nodes = [{ view: [1, 2] }, {}, {}];
  const rtc = mockWebRtc(nodes);
  const distance = index => BigInt(`0x${parseWebRtcDirectMultiaddr(rtc.endpoints[index]).peerId}`) ^ BigInt(`0x${datamap.address}`);
  const holder = distance(1) < distance(2) ? 1 : 2;
  nodes[holder].chunk = datamap.content;
  nodes[3 - holder].delay = method => method === "find_node" ? 2500 : 0;
  const client = new BrowserNetworkClient(rtc.endpoints.slice(0, 1));
  let reader, deadline;
  try {
    reader = await Promise.race([
      client.openPublicFile(datamap.address),
      new Promise((_, reject) => deadline = setTimeout(() => reject(new Error("read waited for lookup round")), 1500)),
    ]);
    assert.equal(reader.size, content.length);
    assert.ok(rtc.requests.some(r => r.node === holder && r.method === "get_chunk"));
  } finally { clearTimeout(deadline); reader?.close(); client.close(); }
});

test("early misses do not exhaust the fallback allowance before a later holder", async () => {
  const nodes = Array.from({ length: 12 }, () => ({
    respond: (_channel, method) => method === "find_node" ? false : undefined,
  }));
  const rtc = mockWebRtc(nodes);
  const target = BigInt(`0x${datamap.address}`);
  const ordered = rtc.endpoints.map((endpoint, index) => ({index,
    distance: BigInt(`0x${parseWebRtcDirectMultiaddr(endpoint).peerId}`) ^ target,
  })).sort((a,b) => a.distance < b.distance ? -1 : 1);
  // Seven misses must not prevent trying a holder already in the bounded
  // candidate set while the ordinary discovery walk is still outstanding.
  const holder = ordered[8].index;
  nodes[holder].chunk = datamap.content;
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader, deadline;
  try {
    reader = await Promise.race([
      client.openPublicFile(datamap.address),
      new Promise((_, reject) => deadline = setTimeout(() => reject(new Error("speculation exhausted before holder")), 1500)),
    ]);
    assert.equal(reader.size, content.length);
    const gets = rtc.requests.filter(r => r.method === "get_chunk");
    assert.ok(gets.some(r => r.node === holder));
    assert.equal(new Set(gets.map(r => r.node)).size, gets.length);
  } finally { clearTimeout(deadline); reader?.close(); client.close(); }
});

test("cold closer hints cannot occupy the read slots before a connected holder", async () => {
  const nodes = [{ view: [1, 2, 3] }, {}, {}, {}];
  const rtc = mockWebRtc(nodes);
  const target = BigInt(`0x${datamap.address}`);
  const ordered = [1, 2, 3].sort((a, b) => {
    const distance = i => BigInt(`0x${parseWebRtcDirectMultiaddr(rtc.endpoints[i]).peerId}`) ^ target;
    return distance(a) < distance(b) ? -1 : 1;
  });
  for (const i of ordered.slice(0, 2)) nodes[i].connectDelay = 2500;
  const holder = ordered[2]; nodes[holder].chunk = datamap.content;
  const client = new BrowserNetworkClient(rtc.endpoints.slice(0, 1));
  let reader, deadline;
  try {
    reader = await Promise.race([
      client.openPublicFile(datamap.address),
      new Promise((_, reject) => deadline = setTimeout(() => reject(new Error("cold hints occupied read slots")), 1500)),
    ]);
    assert.equal(reader.size, content.length);
    assert.ok(rtc.requests.some(r => r.node === holder && r.method === "get_chunk"));
    assert.ok(!rtc.requests.some(r => ordered.slice(0, 2).includes(r.node) && r.method === "get_chunk"));
  } finally { clearTimeout(deadline); reader?.close(); client.close(); }
});

test("shared Client rejects corrupt content immediately, matching native GET", async () => {
  const rtc = mockWebRtc(Array.from({ length: 24 }, () => ({ chunk: new Uint8Array([1, 2, 3]), respond: failDiscovery })));
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    await assert.rejects(client.openPublicFile(datamap.address), /BLAKE3 mismatch/);
    const gets = rtc.requests.filter(request => request.method === "get_chunk");
    // Discovery can finish while the first response yields to the event loop.
    // The shared policy then permits one ordinary GET alongside the early GET.
    assert.ok(gets.length >= 1 && gets.length <= 2, "at most the already-racing GETs may run");
    await new Promise(resolve => setTimeout(resolve, 20));
    assert.equal(rtc.requests.filter(request => request.method === "get_chunk").length, gets.length,
      "integrity failure must cancel the race without fallback or retry");
  } finally { client.close(); }
});


test("a just-uploaded file downloads and streams after its holders fail discovery", async () => {
  const nodes = Array.from({ length: 7 }, () => ({}));
  const rtc = mockWebRtc(nodes);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    const uploaded = await client.uploadPublicFile(content, "roundtrip.txt", "text/plain", paymentNetwork, async (_, quotes) => ({
      transactionHash: `0x${"ab".repeat(32)}`,
      totalAmount: quotes.reduce((sum, quote) => sum + BigInt(quote.amount), 0n).toString(),
    }));
    assert.equal(uploaded.file.replicas, 4);
    const holders = rtc.stores.map((records, index) => records.has(uploaded.file.address) ? index : -1).filter(index => index >= 0);
    assert.equal(holders.length, 4);
    for (const index of holders) nodes[index].respond = failDiscovery;
    const downloaded = await client.downloadPublicFile(uploaded.file, 3);
    assert.deepEqual(downloaded.content, content);
    reader = await client.openPublicFile(uploaded.file.address);
    assert.deepEqual(await reader.readRange(0, content.length), content);
  } finally { reader?.close(); client.close(); }
});

test("native shrunk DataMaps download and stream through the shared async engine", async () => {
  const maxChunkSize = 4_190_208;
  const original = Uint8Array.from({ length: 3 * maxChunkSize + 1 }, (_, index) => index * 37);
  const encrypted = encryptPublicFile(original);
  const rtc = mockWebRtc([{}]);
  for (const record of encrypted.records) rtc.stores[0].set(record.address, record.content);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    const address = encrypted.records.at(-1).address;
    const downloaded = await client.downloadPublicFile(address, 3);
    assert.deepEqual(downloaded.content, original);
    reader = await client.openPublicFile(address);
    const boundary = encrypted.chunks[0].src_size;
    for (const [start, length] of [[0, 32], [boundary - 1, 2], [boundary, 50], [original.length - 10, 100], [original.length, 10], [0, 0]]) {
      assert.deepEqual(await reader.readRange(start, length), original.slice(start, start + length));
    }
    reader.close();
    await assert.rejects(reader.readRange(0, 1), /closed/);
  } finally { reader?.close(); client.close(); }
});

const maxChunkSize = 4_190_208;
const gets = rtc => rtc.requests.filter(request => request.method === "get_chunk").length;
const getsOf = (rtc, address) =>
  rtc.requests.filter(request => request.method === "get_chunk" && request.address === address).length;
async function until(condition, what) {
  const deadline = Date.now() + 10_000;
  while (!condition()) {
    assert(Date.now() < deadline, `timed out waiting for ${what}`);
    await new Promise(resolve => setTimeout(resolve, 5));
  }
}
// Background read-ahead has settled once no GET has been sent for `quietMs`.
async function settled(rtc, quietMs = 100) {
  const deadline = Date.now() + 10_000;
  for (let count = -1; count !== gets(rtc);) {
    assert(Date.now() < deadline, `read-ahead never settled after ${gets(rtc)} GETs`);
    count = gets(rtc);
    await new Promise(resolve => setTimeout(resolve, quietMs));
  }
  return gets(rtc);
}
function recordFile(records) {
  const original = Uint8Array.from({ length: records * maxChunkSize }, (_, index) => index * 31);
  const encrypted = encryptPublicFile(original);
  const starts = encrypted.chunks.map((_, index) =>
    encrypted.chunks.slice(0, index).reduce((sum, chunk) => sum + chunk.src_size, 0));
  return { original, encrypted, starts };
}
// `delays` maps record indices to GET response delays in milliseconds. The node
// multiplexes, as real nodes do, so a slow GET does not hold up the others.
function serve(encrypted, missing = new Set(), delays = new Map()) {
  const delay = new Map([...delays].map(([index, ms]) => [encrypted.chunks[index].dst_hash, ms]));
  const rtc = mockWebRtc([{
    multiplex: true,
    delay: (method, address) => (method === "get_chunk" && delay.get(address)) || 0,
  }]);
  for (const record of encrypted.records) {
    if (!missing.has(record.address)) rtc.stores[0].set(record.address, record.content);
  }
  return rtc;
}
let largeFile;
// Larger than the read-ahead window plus the last record.
const large = () => (largeFile ??= recordFile(12));

test("a streaming reader reads ahead from any read; an ordinary one only fetches the next record", async () => {
  const { original, encrypted, starts } = recordFile(5);
  for (const streaming of [true, false]) {
    const rtc = serve(encrypted);
    const client = new BrowserNetworkClient(rtc.endpoints);
    let reader;
    try {
      reader = await client.openPublicFile(encrypted.address, undefined, { streaming });
      assert.deepEqual(await reader.readRange(starts[1], 10), original.slice(starts[1], starts[1] + 10));
      const before = await settled(rtc);
      assert.deepEqual(await reader.readRange(starts[3], 10), original.slice(starts[3], starts[3] + 10));
      assert.equal(gets(rtc) === before, streaming, `streaming=${streaming}`);
    } finally { reader?.close(); client.close(); }
  }
});

test("streaming read-ahead fetches each record of a file larger than its window once, then goes quiet", async () => {
  const { original, encrypted, starts } = large();
  const rtc = serve(encrypted);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(encrypted.address, undefined, { streaming: true });
    const opened = gets(rtc);
    for (const start of starts) {
      assert.deepEqual(await reader.readRange(start, 10), original.slice(start, start + 10));
      await settled(rtc);
    }
    assert.equal(await settled(rtc, 500) - opened, starts.length);
  } finally { reader?.close(); client.close(); }
});

test("an ordinary reader reads ahead only once a read continues another", async () => {
  const { original, encrypted, starts } = large();
  const rtc = serve(encrypted);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(encrypted.address);
    const opened = gets(rtc);
    // A header read fetches its record and the next, not the window or the end.
    assert.deepEqual(await reader.readRange(0, 10), original.slice(0, 10));
    assert.equal(await settled(rtc, 500) - opened, 2);
    assert.deepEqual(await reader.readRange(10, 10), original.slice(10, 20));
    const window = await settled(rtc, 500) - opened;
    assert(window > 2 && window < starts.length, `fetched ${window} records`);
  } finally { reader?.close(); client.close(); }
});

test("a zero-length read is not a read that a later one continues", async () => {
  const { original, encrypted } = large();
  const rtc = serve(encrypted);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(encrypted.address);
    const opened = gets(rtc);
    assert.equal((await reader.readRange(0, 0)).length, 0);
    assert.equal(await settled(rtc, 500), opened);
    // The header read starts no window: its record and the next only.
    assert.deepEqual(await reader.readRange(0, 10), original.slice(0, 10));
    assert.equal(await settled(rtc, 500) - opened, 2);
  } finally { reader?.close(); client.close(); }
});

test("a seek's read is not queued behind read-ahead GETs still in flight", async () => {
  // The window's GETs take far longer than the seek target's. Read-ahead GETs
  // hold at most a share of the read budget, so the seek's GET is admitted at
  // once, even while GETs of fetches the seek cancels still drain.
  const slow = 3_000;
  const { original, encrypted, starts } = large();
  const rtc = serve(encrypted, new Set(), new Map([1, 2, 3, 4].map(index => [index, slow])));
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(encrypted.address, undefined, { streaming: true });
    assert.deepEqual(await reader.readRange(0, 10), original.slice(0, 10));
    // Let read-ahead send every GET the read budget admits it.
    await settled(rtc, 200);
    assert(getsOf(rtc, encrypted.chunks[1].dst_hash) > 0, "the window is not being fetched");
    const seeked = Date.now();
    assert.deepEqual(await reader.readRange(starts[8], 10), original.slice(starts[8], starts[8] + 10));
    const elapsed = Date.now() - seeked;
    assert(elapsed < slow / 2, `the seek waited ${elapsed} ms`);
    await settled(rtc, slow);
  } finally { reader?.close(); client.close(); }
});

test("a read fetches the records read-ahead refuses it alongside those it admits", async () => {
  // Two reads that continue each other start a window whose four fetches stay
  // in flight, so a read spanning records 9 and 10 gets read-ahead for one of
  // them only. It fetches the other at the same time, not after.
  const window = 2_500;
  const record = 1_000;
  const { original, encrypted, starts } = large();
  const delays = new Map([[1, window], [2, window], [3, window], [4, window], [9, record], [10, record]]);
  const rtc = serve(encrypted, new Set(), delays);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(encrypted.address);
    assert.deepEqual(await reader.readRange(0, 10), original.slice(0, 10));
    assert.deepEqual(await reader.readRange(10, 10), original.slice(10, 20));
    const start = starts[10] - 5;
    const began = Date.now();
    assert.deepEqual(await reader.readRange(start, 10), original.slice(start, start + 10));
    const elapsed = Date.now() - began;
    assert(elapsed < record * 1.8, `the read took ${elapsed} ms`);
    await settled(rtc, window);
  } finally { reader?.close(); client.close(); }
});

test("a read in progress keeps its records when another read needs room", async () => {
  // A window keeps four fetches in flight; a read of record 11 takes the fifth.
  // A second read then needs room. It must not cancel the first read's fetch,
  // whose GET is already on the wire, so record 11 is fetched once.
  const window = 2_000;
  const { original, encrypted, starts } = large();
  const delays = new Map([[1, window], [2, window], [3, window], [4, window], [11, window / 2]]);
  const rtc = serve(encrypted, new Set(), delays);
  const client = new BrowserNetworkClient(rtc.endpoints);
  const last = encrypted.chunks[11].dst_hash;
  let reader;
  try {
    reader = await client.openPublicFile(encrypted.address);
    assert.deepEqual(await reader.readRange(0, 10), original.slice(0, 10));
    assert.deepEqual(await reader.readRange(10, 10), original.slice(10, 20));
    const first = reader.readRange(starts[11], 10);
    await until(() => getsOf(rtc, last) > 0, "the first read's GET");
    const second = reader.readRange(starts[10], 10);
    assert.deepEqual(await first, original.slice(starts[11], starts[11] + 10));
    assert.deepEqual(await second, original.slice(starts[10], starts[10] + 10));
    assert.equal(getsOf(rtc, last), 1);
    await settled(rtc, window);
  } finally { reader?.close(); client.close(); }
});

test("a DataMap whose records range reads reject still opens", async () => {
  const records = [0, 1, 2].map(index => {
    const content = Uint8Array.from({ length: 1024 }, (_, byte) => (byte + index) & 0xff);
    return { address: contentAddress(content), content };
  });
  // Indices 0, 1 and 3: not contiguous.
  const map = test_encode_public_data_map(records.map((record, position) => ({
    index: position < 2 ? position : 3, dst_hash: record.address, src_hash: "00".repeat(32), src_size: 1024,
  })));
  const rtc = mockWebRtc([{}]);
  for (const record of [...records, map]) rtc.stores[0].set(record.address, record.content);
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(map.address, undefined, { streaming: true });
    assert.equal(reader.size, 3 * 1024);
    await assert.rejects(reader.readRange(0, 10), /contiguous/);
  } finally { reader?.close(); client.close(); }
});

test("read-ahead retries a failed record only after another read", async () => {
  const { original, encrypted, starts } = recordFile(6);
  const rtc = serve(encrypted, new Set([encrypted.chunks[3].dst_hash]));
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(encrypted.address, undefined, { streaming: true });
    assert.deepEqual(await reader.readRange(0, 10), original.slice(0, 10));
    // A failed read waits a second before its retry round; settle past it.
    const failed = await settled(rtc, 1_500);
    await new Promise(resolve => setTimeout(resolve, 2_500));
    assert.equal(gets(rtc), failed, "an idle reader retried a failed record");
    assert.deepEqual(await reader.readRange(starts[1], 10), original.slice(starts[1], starts[1] + 10));
    assert(await settled(rtc, 1_500) > failed, "the next read retried the failed record");
  } finally { reader?.close(); client.close(); }
});

// Forty records that each declare one KiB, so all of them overlap the 32 MiB
// window. None decrypts. The first record's GET is slow, so read-ahead runs
// while the reads that need it wait.
const declared = 1024;
const maxHeld = 24;
function misdeclaredMap() {
  const records = Array.from({ length: 40 }, (_, index) => {
    const content = Uint8Array.from({ length: 16 * 1024 }, (_, byte) => (byte * 7 + index) & 0xff);
    return { address: contentAddress(content), content };
  });
  const map = test_encode_public_data_map(records.map((record, index) => ({
    index, dst_hash: record.address, src_hash: "00".repeat(32), src_size: declared,
  })));
  const slow = records[0].address;
  const rtc = mockWebRtc([{
    multiplex: true,
    delay: (method, address) => method !== "get_chunk" ? 0 : address === slow ? 1_500 : 20,
  }]);
  for (const record of [...records, map]) rtc.stores[0].set(record.address, record.content);
  return { records, map, rtc };
}
// Sample the readers' read-ahead until no GET has been sent for a while.
async function watch(rtc, readers, peak) {
  const deadline = Date.now() + 10_000;
  for (let count = -1, quietSince = Date.now(); ;) {
    const usage = readers.map(reader => reader.testReadAhead());
    peak.fetching = Math.max(peak.fetching, ...usage.map(({ fetching }) => fetching));
    peak.total = Math.max(peak.total, usage.reduce((sum, { fetching, held }) => sum + fetching + held, 0));
    if (gets(rtc) !== count) [count, quietSince] = [gets(rtc), Date.now()];
    else if (Date.now() - quietSince >= 200) return count;
    assert(Date.now() < deadline, `read-ahead never settled after ${gets(rtc)} GETs`);
    await new Promise(resolve => setTimeout(resolve, 5));
  }
}

test("read-ahead of a misdeclared DataMap stays within its record bounds and stops when a read fails", async () => {
  // Read-ahead counts records, not declared bytes: at most the window cap of
  // ten plus the last record, five in flight and 24 held or in flight.
  const maxReadAhead = 11;
  const { records, map, rtc } = misdeclaredMap();
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  const peak = { fetching: 0, total: 0 };
  try {
    reader = await client.openPublicFile(map.address, undefined, { streaming: true });
    const opened = gets(rtc);
    const first = assert.rejects(reader.readRange(0, 1));
    const fetched = await watch(rtc, [reader], peak) - opened;
    assert.equal(fetched, maxReadAhead, `read ahead ${fetched} records`);
    // A read needing every record admits only what fits; it fetches the rest.
    const failing = assert.rejects(reader.readRange(0, records.length * declared));
    await watch(rtc, [reader], peak);
    await Promise.all([first, failing]);
    assert(peak.fetching <= 5, `${peak.fetching} records in flight`);
    assert(peak.total <= maxHeld, `${peak.total} records held or in flight`);
    // The records do not decrypt, so read-ahead stops and releases them. A
    // later read fetches only its own record.
    assert.deepEqual(reader.testReadAhead(), { fetching: 0, held: 0 });
    const stopped = gets(rtc);
    await assert.rejects(reader.readRange(5 * declared, 1));
    assert.equal(await watch(rtc, [reader], peak) - stopped, 1);
  } finally { reader?.close(); client.close(); }
});

test("the readers of one client share its read-ahead budget", async () => {
  // Three streaming readers each want their window and the last record, eleven
  // records, but together hold or fetch at most 24. Each reader holds its own
  // records, while those they read ahead at the same time are fetched once.
  const { map, rtc } = misdeclaredMap();
  const client = new BrowserNetworkClient(rtc.endpoints);
  const readers = [];
  const peak = { fetching: 0, total: 0 };
  try {
    for (let index = 0; index < 3; index += 1) {
      readers.push(await client.openPublicFile(map.address, undefined, { streaming: true }));
    }
    const opened = gets(rtc);
    const reads = readers.map(reader => assert.rejects(reader.readRange(0, 1)));
    const fetched = await watch(rtc, readers, peak) - opened;
    assert(peak.total <= maxHeld, `${peak.total} records held or in flight`);
    assert(peak.total > 11, `only ${peak.total} records were held or in flight`);
    assert(fetched <= 11, `${fetched} GETs for the eleven records the readers share`);
    await Promise.all(reads);
  } finally {
    for (const reader of readers) reader.close();
    client.close();
  }
});

test("omitting the download cap selects the shared adaptive scheduler", async () => {
  const rtc = mockWebRtc([{}]);
  for (const record of encrypted.records) rtc.stores[0].set(record.address, record.content);
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    const result = await client.downloadPublicFile(datamap.address, undefined);
    assert.deepEqual(result.content, content);
  } finally { client.close(); }
});
