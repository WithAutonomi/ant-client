import assert from "node:assert/strict";
import test from "node:test";
import { readFile, mkdtemp, rm } from "node:fs/promises";
import { createWriteStream } from "node:fs";
import { Writable } from "node:stream";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { encryptPublicFile, parseWebRtcDirectMultiaddr } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

const large = JSON.parse(await readFile(new URL("./fixtures/large-file.json", import.meta.url)));
const records = large.records.map(record => ({
  address: record.address, content: Uint8Array.from(Buffer.from(record.content, "hex")),
}));
const map = records.find(record => record.address === large.address);

function fixture(source = records, node = {}) {
  const rtc = mockWebRtc([node]);
  for (const record of source) rtc.stores[0].set(record.address, record.content);
  return { rtc, client: new BrowserNetworkClient(rtc.endpoints) };
}

const gets = rtc => rtc.requests.filter(r => r.method === "get_chunk").length;

// Distinct, incompressible chunks, so every chunk is its own network GET.
function randomBytes(length) {
  const bytes = new Uint8Array(length);
  let state = 0x9e3779b9;
  for (let i = 0; i < length; i++) {
    state ^= state << 13; state ^= state >>> 17; state ^= state << 5;
    bytes[i] = state;
  }
  return bytes;
}

test("native files larger than 4 GiB support exact public and private seeks", async () => {
  const { rtc, client } = fixture();
  try {
    for (const open of [() => client.openPublicFile(large.address),
      () => client.openPrivateFile({ data_map: map.content })]) {
      const reader = await open();
      try {
        assert.equal(reader.size, large.size);
        assert(reader.size > 2 ** 32);
        // Cross both the old 1 GB cap and the wasm32 position boundary.
        for (const start of [1_000_000_000, 2 ** 32 - 8, 2 ** 32 + 17, large.size - 8]) {
          const bytes = await reader.readRange(start, 32);
          assert.deepEqual(bytes, new Uint8Array(Math.min(32, large.size - start)).fill(large.byte));
        }
        assert.equal((await reader.readRange(large.size, 1)).length, 0);
        assert.equal((await reader.readRange(Number.MAX_SAFE_INTEGER, 1)).length, 0);
        const before = gets(rtc);
        for (const value of [-1, 0.5, NaN, Infinity, 2 ** 53]) {
          await assert.rejects(reader.readRange(value, 1), /safe integer/);
          await assert.rejects(reader.readRange(0, value), /safe integer/);
        }
        await assert.rejects(reader.readRange(0, 2 ** 32 + 1), /range reads are limited/);
        assert.equal(gets(rtc), before);
      } finally { reader.close(); }
    }
  } finally { client.close(); }
});

test("a complete native >4 GiB file streams with bounded writes and matching BLAKE3", { timeout: 180_000 }, async () => {
  const { client } = fixture();
  let reader;
  let written = 0, largest = 0, closed = false;
  try {
    reader = await client.openPublicFile(large.address);
    const sink = new WritableStream({
      write(bytes) {
        written += bytes.length;
        largest = Math.max(largest, bytes.length);
        assert.equal(bytes[0], large.byte);
        assert.equal(bytes.at(-1), large.byte);
      },
      close() { closed = true; },
    });
    const result = await reader.pipeTo(sink);
    assert.equal(written, large.size);
    assert.equal(result.bytesWritten, large.size);
    assert.equal(result.hash, large.hash);
    assert(largest <= 4 * 1024 * 1024);
    assert(closed);
    assert.equal(sink.locked, false);
  } finally { reader?.close(); client.close(); }
});

test("large-file memory budgets fail before content fetch, and allocation errors permit streaming", async () => {
  // A budget rejection makes exactly the requests that resolving the file does.
  const opened = fixture();
  try { (await opened.client.openPublicFile(large.address)).close(); } finally { opened.client.close(); }
  const maxMemoryBytes = 64 * 1024 * 1024;
  for (const download of [
    client => client.downloadPublicFile(large.address, undefined, undefined, { maxMemoryBytes }),
    client => client.downloadPrivateFile({ data_map: map.content }, undefined, undefined, { maxMemoryBytes }),
  ]) {
    const { rtc, client } = fixture();
    try {
      await assert.rejects(download(client), /memory budget.*pipeTo/);
      assert(gets(rtc) <= gets(opened.rtc), `${gets(rtc)} GETs for a budget rejection`);
    } finally { client.close(); }
  }

  const { rtc, client } = fixture();
  try {
    for (const value of [-1, 1.5, Infinity, NaN, 2 ** 53]) {
      await assert.rejects(client.downloadPublicFile(large.address, undefined, undefined, { maxMemoryBytes: value }), /safe integer/);
    }
    await assert.rejects(client.downloadPublicFile(large.address, 0), /positive integer/);
    await assert.rejects(client.downloadPublicFile(large.address, undefined, undefined, { signal: {} }), /AbortSignal/);
    assert.equal(gets(rtc), 0, "invalid arguments fail before any fetch");
  } finally { client.close(); }
});

test("an AbortSignal cancels a complete download, including during retry waits", async () => {
  const original = new TextEncoder().encode("Download cancellation.".repeat(300));
  const encrypted = encryptPublicFile(original);
  const missing = encrypted.records.find(record => record.address !== encrypted.address);
  const { rtc, client } = fixture(encrypted.records.filter(record => record !== missing));
  try {
    const reason = new Error("stop downloading");
    await assert.rejects(client.downloadPublicFile(encrypted.address, undefined, undefined, { signal: AbortSignal.abort(reason) }),
      error => error === reason);
    const controller = new AbortController();
    const started = Date.now();
    const pending = client.downloadPublicFile(encrypted.address, undefined, undefined, { signal: controller.signal });
    setTimeout(() => controller.abort(reason), 100);
    await assert.rejects(pending, error => error === reason);
    assert(Date.now() - started < 5_000, `abort took ${Date.now() - started} ms`);
    rtc.stores[0].set(missing.address, missing.content);
    assert.deepEqual((await client.downloadPublicFile(encrypted.address)).content, original);
  } finally { client.close(); }
});

test("pipeTo writes ranges beyond 4 GiB to disk", async () => {
  const { client } = fixture();
  const directory = await mkdtemp(join(tmpdir(), "ant-wasm-download-"));
  let reader;
  try {
    reader = await client.openPublicFile(large.address);
    const path = join(directory, "tail.bin");
    const sink = Writable.toWeb(createWriteStream(path));
    const start = 2 ** 32 - 8;
    const result = await reader.pipeTo(sink, { start });
    assert.equal(result.bytesWritten, large.size - start);
    assert.deepEqual(await readFile(path), Buffer.alloc(large.size - start, large.byte));
    assert.equal(sink.locked, false);
  } finally { reader?.close(); client.close(); await rm(directory, { recursive: true, force: true }); }
});

test("pipeTo keeps fetching behind a blocked write only up to its 32 MiB buffer", async () => {
  // Twelve distinct chunks: about 50 MB, more than one buffer.
  const original = randomBytes(12 * 4_190_208);
  const encrypted = encryptPublicFile(original);
  // Slow GETs are still in flight when the first write blocks.
  const { rtc, client } = fixture(encrypted.records, { delay: method => method === "get_chunk" ? 30 : 0 });
  let reader;
  try {
    reader = await client.openPublicFile(encrypted.address);
    let unblock, firstWrite;
    const entered = new Promise(resolve => { firstWrite = resolve; });
    const gate = new Promise(resolve => { unblock = resolve; });
    const chunks = [];
    let writes = 0;
    const sink = new WritableStream({
      async write(bytes) {
        if (++writes === 1) { firstWrite(); await gate; }
        chunks.push(bytes);
      },
    });
    const before = gets(rtc);
    const completion = reader.pipeTo(sink);
    await Promise.race([entered, completion.then(() => assert.fail("pipeTo finished without writing"))]);
    // Fetching continues behind the blocked write until the buffer is full,
    // and stops there.
    const bufferChunks = Math.floor(32 * 1024 * 1024 / 4_190_208);
    const deadline = Date.now() + 10_000;
    while (gets(rtc) - before < bufferChunks && Date.now() < deadline) {
      await new Promise(resolve => setTimeout(resolve, 20));
    }
    assert.equal(gets(rtc) - before, bufferChunks, "chunks fetched behind one blocked write");
    await new Promise(resolve => setTimeout(resolve, 200));
    assert.equal(gets(rtc) - before, bufferChunks, "fetching stops at the buffer bound");
    unblock();
    const result = await completion;
    assert.equal(result.bytesWritten, original.length);
    assert.deepEqual(Buffer.concat(chunks), Buffer.from(original));
    assert(chunks.every(chunk => chunk.length <= 4 * 1024 * 1024));
  } finally { reader?.close(); client.close(); }
});

test("invalid pipeTo calls reject without touching the destination", async () => {
  const { client } = fixture();
  let reader;
  try {
    reader = await client.openPublicFile(large.address);
    for (const [options, expected, close] of [
      [{ end: large.size + 1 }, /outside the file/],
      [{ start: 10, end: 5 }, /outside the file/],
      [{ start: -1 }, /safe integer/],
      [{ onProgress: "no" }, /onProgress must be a function/],
      [{ signal: {} }, /AbortSignal/],
      [{ signal: new EventTarget() }, /AbortSignal/],
      [{}, /closed/, true],
    ]) {
      if (close) reader.close();
      let aborted = false, closed = false;
      const sink = new WritableStream({ write() { assert.fail("nothing is written"); }, close() { closed = true; }, abort() { aborted = true; } });
      await assert.rejects(reader.pipeTo(sink, options), expected);
      assert.equal(aborted, false);
      assert.equal(closed, false);
      assert.equal(sink.locked, false);
    }
  } finally { reader?.close(); client.close(); }
});

test("an AbortSignal cancels one pipeTo and leaves its reader open", async () => {
  const { client } = fixture();
  let reader;
  try {
    reader = await client.openPublicFile(large.address);
    const controller = new AbortController();
    const reason = new Error("user cancelled");
    let writes = 0, aborted;
    const sink = new WritableStream({
      write() { writes++; controller.abort(reason); },
      abort(error) { aborted = error; },
    });
    await assert.rejects(reader.pipeTo(sink, { start: 2 ** 32, signal: controller.signal }), error => error === reason);
    assert.equal(writes, 1);
    assert.equal(aborted, reason);
    assert.equal(sink.locked, false);

    const early = new WritableStream({ write() { assert.fail("an aborted signal writes nothing"); } });
    await assert.rejects(reader.pipeTo(early, { signal: AbortSignal.abort(reason) }), error => error === reason);
    assert.equal(early.locked, false);
    assert.deepEqual(await reader.readRange(2 ** 32, 4), new Uint8Array(4).fill(large.byte));
  } finally { reader?.close(); client.close(); }
});

test("an AbortSignal stops a pipeTo waiting to retry a missing record", async () => {
  const original = new TextEncoder().encode("Retry cancellation.".repeat(300));
  const encrypted = encryptPublicFile(original);
  const missing = encrypted.records.find(record => record.address !== encrypted.address);
  const { client } = fixture(encrypted.records.filter(record => record !== missing));
  let reader;
  try {
    reader = await client.openPublicFile(encrypted.address);
    const controller = new AbortController();
    const started = Date.now();
    const pending = reader.pipeTo(new WritableStream(), { signal: controller.signal });
    setTimeout(() => controller.abort(new Error("stop")), 100);
    await assert.rejects(pending, /stop/);
    assert(Date.now() - started < 5_000, `abort took ${Date.now() - started} ms`);
  } finally { reader?.close(); client.close(); }
});

test("an AbortSignal settles pipeTo even while a destination write is stalled", async () => {
  const { client } = fixture();
  let reader;
  try {
    reader = await client.openPublicFile(large.address);
    const controller = new AbortController();
    const reason = new Error("stalled destination");
    let writing;
    const stalled = new Promise(resolve => { writing = resolve; });
    const sink = new WritableStream({ write() { writing(); return new Promise(() => {}); } });
    const pending = reader.pipeTo(sink, { signal: controller.signal });
    await stalled;
    const started = Date.now();
    controller.abort(reason);
    await assert.rejects(pending, error => error === reason);
    assert(Date.now() - started < 2_000, `abort took ${Date.now() - started} ms`);
  } finally { reader?.close(); client.close(); }
});

test("an AbortSignal during the destination's close rejects pipeTo", async () => {
  const original = new TextEncoder().encode("Abort while committing.".repeat(200));
  const encrypted = encryptPublicFile(original);
  const { client } = fixture(encrypted.records);
  let reader;
  try {
    reader = await client.openPublicFile(encrypted.address);
    const controller = new AbortController();
    const reason = new Error("cancelled during commit");
    let closing;
    const committing = new Promise(resolve => { closing = resolve; });
    const sink = new WritableStream({ close() { closing(); return new Promise(() => {}); } });
    const pending = reader.pipeTo(sink, { signal: controller.signal });
    await committing;
    controller.abort(reason);
    await assert.rejects(pending, error => error === reason);
  } finally { reader?.close(); client.close(); }
});

test("a writer rejection stops a stream and releases its lock", async () => {
  const { client } = fixture();
  let reader;
  try {
    reader = await client.openPublicFile(large.address);
    let writes = 0, closed = false;
    const failure = new Error("disk full");
    const sink = new WritableStream({ write() { writes++; throw failure; }, close() { closed = true; } });
    await assert.rejects(reader.pipeTo(sink, { start: 2 ** 32 }), error => error === failure);
    assert.equal(writes, 1);
    assert.equal(closed, false);
    assert.equal(sink.locked, false);
  } finally { reader?.close(); client.close(); }
});

test("in-memory downloads use one JS output buffer and return recoverable allocation failures", async () => {
  const original = new TextEncoder().encode("In-memory download round trip.".repeat(100));
  const encrypted = encryptPublicFile(original);
  const { rtc, client } = fixture(encrypted.records);
  const NativeUint8Array = globalThis.Uint8Array;
  let reader;
  try {
    const messages = [];
    const result = await client.downloadPublicFile(encrypted.address, 2, message => messages.push(message),
      { maxMemoryBytes: original.length });
    assert.deepEqual(result.content, original);
    assert.equal(result.file.chunks.length, 3);
    // Probes time the first chunks by these messages, and a read investigation
    // needs the node that served each chunk and the GETs it cost (V2-1872).
    const peers = rtc.endpoints.map(endpoint => parseWebRtcDirectMultiaddr(endpoint).peerId);
    const chunkProgress = messages.filter(message => message.startsWith("Downloaded chunk "));
    assert.equal(chunkProgress[0], "Downloaded chunk 0/3");
    assert.equal(chunkProgress.length, 4);
    chunkProgress.slice(1).forEach((message, delivered) => {
      const parts = /^Downloaded chunk (\d+)\/3 from ([0-9a-f]{64}) after (\d+) attempts$/.exec(message);
      assert(parts, `unexpected chunk progress ${message}`);
      assert.equal(parts[1], String(delivered + 1));
      assert(peers.includes(parts[2]), `${parts[2]} served a chunk but is not a mock node`);
      // One uncontended mock holder: one GET per chunk.
      assert.equal(parts[3], "1");
    });
    const unset = await client.downloadPublicFile(encrypted.address, null, () => {});
    assert.equal(unset.hash, result.hash);
    globalThis.Uint8Array = class extends NativeUint8Array {
      constructor(...args) {
        if (args.length === 1 && args[0] === original.length) throw new RangeError("allocation refused");
        super(...args);
      }
    };
    await assert.rejects(client.downloadPublicFile(encrypted.address), /cannot allocate download buffer.*pipeTo/);
    // An output failure is returned as the output's own error, not as bad data.
    const refused = new RangeError("output refused");
    globalThis.Uint8Array = class extends NativeUint8Array {
      subarray(...args) {
        if (this.length === original.length) throw refused;
        return super.subarray(...args);
      }
    };
    await assert.rejects(client.downloadPublicFile(encrypted.address), error => error === refused);
    globalThis.Uint8Array = NativeUint8Array;
    reader = await client.openPublicFile(encrypted.address);
    const chunks = [];
    const streamed = await reader.pipeTo(new WritableStream({ write(bytes) { chunks.push(bytes); } }));
    assert.equal(streamed.hash, result.hash);
    assert.deepEqual(Buffer.concat(chunks), Buffer.from(original));
  } finally { globalThis.Uint8Array = NativeUint8Array; reader?.close(); client.close(); }
});

test("a repeated in-memory download reports every chunk from the chunk cache", async () => {
  const original = new TextEncoder().encode("Cached download round trip.".repeat(100));
  const encrypted = encryptPublicFile(original);
  const { rtc, client } = fixture(encrypted.records);
  try {
    await client.downloadPublicFile(encrypted.address, 2, () => {});
    const before = rtc.requests.length;
    const messages = [];
    const again = await client.downloadPublicFile(encrypted.address, 2, message => messages.push(message));
    assert.deepEqual(again.content, original);
    // The client's chunk cache answers the second download, so it reports no
    // serving peer and no GET.
    assert.deepEqual(messages.filter(message => message.startsWith("Downloaded chunk ")), [
      "Downloaded chunk 0/3",
      "Downloaded chunk 1/3 from cache after 0 attempts",
      "Downloaded chunk 2/3 from cache after 0 attempts",
      "Downloaded chunk 3/3 from cache after 0 attempts",
    ]);
    // And it really sent none: the data records are the file's records other
    // than its DataMap, which the download resolves on its own path.
    const data = new Set(encrypted.records.map(record => record.address)
      .filter(address => address !== encrypted.address));
    const refetched = rtc.requests.slice(before)
      .filter(request => request.method === "get_chunk" && data.has(request.address));
    assert.deepEqual(refetched, []);
  } finally { client.close(); }
});

test("a chunk retried in a later round reports every GET it cost", { timeout: 120_000 }, async () => {
  const original = new TextEncoder().encode("Retried download round trip.".repeat(100));
  const encrypted = encryptPublicFile(original);
  const withheld = encrypted.records.find(record => record.address !== encrypted.address);
  // Answer the first two GETs for one data record from a store that does not
  // hold it. One `chunk_get` call cannot spend more than that before giving up
  // here, so the record is only found in a later deferred round of
  // `read_chunks` — which is a fresh `chunk_get` with its own attempt count.
  const WITHHELD_GETS = 2;
  let asked = 0;
  let rtc;
  rtc = mockWebRtc([{
    respond(_channel, method) {
      if (method !== "get_chunk") return;
      if (rtc.requests.at(-1).address !== withheld.address) return;
      // The NotFound is already built, so supplying the record now makes it
      // available to the next GET, not this one.
      if (++asked === WITHHELD_GETS) rtc.stores[0].set(withheld.address, withheld.content);
    },
  }]);
  for (const record of encrypted.records) {
    if (record.address !== withheld.address) rtc.stores[0].set(record.address, record.content);
  }
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    const messages = [];
    const result = await client.downloadPublicFile(encrypted.address, 2, message => messages.push(message));
    assert.deepEqual(result.content, original);
    const served = rtc.requests
      .filter(request => request.method === "get_chunk" && request.address === withheld.address).length;
    // More GETs than one round can spend: the record was fetched again after a
    // round returned NotFound, so the reported total has to span both.
    assert(served > WITHHELD_GETS, `the withheld record was only asked for ${served} time(s)`);
    const attempts = messages
      .filter(message => message.startsWith("Downloaded chunk ") && message.includes(" after "))
      .map(message => Number(/ after (\d+) attempts$/.exec(message)[1]));
    assert.equal(attempts.length, 3);
    // The retried chunk must report every GET the mock served for it, across
    // the rounds that failed and the round that succeeded — not just the
    // succeeding round's count.
    assert.deepEqual(attempts.filter(count => count > 1), [served]);
    assert.equal(attempts.filter(count => count === 1).length, 2);
  } finally { client.close(); }
});

test("downloads sharing a failed round are both charged for it", { timeout: 120_000 }, async () => {
  const original = new TextEncoder().encode("Shared retry round trip.".repeat(100));
  const encrypted = encryptPublicFile(original);
  const withheld = encrypted.records.find(record => record.address !== encrypted.address);
  const WITHHELD_GETS = 2;
  let asked = 0;
  let rtc;
  rtc = mockWebRtc([{
    // Hold every GET open long enough that two concurrent downloads of the
    // same file always overlap, so the second joins the first's fetch of a
    // record instead of sending its own.
    delay: method => method === "get_chunk" ? 60 : 0,
    respond(_channel, method) {
      if (method !== "get_chunk") return;
      if (rtc.requests.at(-1).address !== withheld.address) return;
      if (++asked === WITHHELD_GETS) rtc.stores[0].set(withheld.address, withheld.content);
    },
  }]);
  for (const record of encrypted.records) {
    if (record.address !== withheld.address) rtc.stores[0].set(record.address, record.content);
  }
  const client = new BrowserNetworkClient(rtc.endpoints);
  const totals = message => Number(/ after (\d+) attempts$/.exec(message)?.[1] ?? NaN);
  try {
    const first = [], second = [];
    const [a, b] = await Promise.all([
      client.downloadPublicFile(encrypted.address, 2, message => first.push(message)),
      client.downloadPublicFile(encrypted.address, 2, message => second.push(message)),
    ]);
    assert.deepEqual(a.content, original);
    assert.deepEqual(b.content, original);
    const served = rtc.requests
      .filter(request => request.method === "get_chunk" && request.address === withheld.address).length;
    assert(served > WITHHELD_GETS, `the withheld record was only asked for ${served} time(s)`);
    const reported = [first, second].map(messages => messages
      .filter(message => message.includes(" after "))
      .map(totals)
      .sort((x, y) => x - y));
    // Both downloads sent the same GETs for the withheld record, because one
    // joined the other's fetch of it — including the round that found nothing.
    // Charging the joiner only for the round that succeeded would report 1.
    assert.equal(reported[0].at(-1), served, `first download reported ${reported[0]}`);
    assert.deepEqual(reported[0], reported[1]);
  } finally { client.close(); }
});
