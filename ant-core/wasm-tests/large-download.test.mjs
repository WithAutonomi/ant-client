import assert from "node:assert/strict";
import test from "node:test";
import { readFile, mkdtemp, rm } from "node:fs/promises";
import { createWriteStream } from "node:fs";
import { Writable } from "node:stream";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { encryptPublicFile } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

const large = JSON.parse(await readFile(new URL("./fixtures/large-file.json", import.meta.url)));
const records = large.records.map(record => ({
  address: record.address, content: Uint8Array.from(Buffer.from(record.content, "hex")),
}));
const map = records.find(record => record.address === large.address);

function fixture(source = records) {
  const rtc = mockWebRtc([{}]);
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
    client => client.downloadPublicFile(large.address, { maxMemoryBytes }),
    client => client.downloadPrivateFile({ data_map: map.content }, { maxMemoryBytes }),
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
      await assert.rejects(client.downloadPublicFile(large.address, { maxMemoryBytes: value }), /safe integer/);
    }
    for (const concurrency of [0, -1, 1.5, NaN]) {
      await assert.rejects(client.downloadPublicFile(large.address, { concurrency }), /positive integer/);
    }
    // The legacy numeric argument keeps wasm-bindgen's `>>> 0` coercion.
    for (const concurrency of [0, NaN, "none", 2 ** 32]) {
      await assert.rejects(client.downloadPublicFile(large.address, concurrency), /positive integer/);
    }
    await assert.rejects(client.downloadPublicFile(large.address, { maxMemory: 1 }), /unknown option `maxMemory`/);
    await assert.rejects(client.downloadPublicFile(large.address, { onProgress: 1 }), /onProgress must be a function/);
    await assert.rejects(client.downloadPublicFile(large.address, { onProgress() {} }, () => {}), /not both/);
    await assert.rejects(client.downloadPublicFile(large.address, { signal: {} }), /AbortSignal/);
    assert.equal(gets(rtc), 0, "invalid options fail before any fetch");
  } finally { client.close(); }
});

test("download options stay optional in the generated typings", async () => {
  const typings = await readFile(new URL("./pkg/ant_core.d.ts", import.meta.url), "utf8");
  for (const name of ["downloadPublicFile", "downloadPrivateFile"]) {
    assert.match(typings, new RegExp(`${name}\\(file: any, options\\?: any`));
  }
});

test("an AbortSignal cancels a complete download, including during retry waits", async () => {
  const original = new TextEncoder().encode("Download cancellation.".repeat(300));
  const encrypted = encryptPublicFile(original);
  const missing = encrypted.records.find(record => record.address !== encrypted.address);
  const { rtc, client } = fixture(encrypted.records.filter(record => record !== missing));
  try {
    const reason = new Error("stop downloading");
    await assert.rejects(client.downloadPublicFile(encrypted.address, { signal: AbortSignal.abort(reason) }),
      error => error === reason);
    const controller = new AbortController();
    const started = Date.now();
    const pending = client.downloadPublicFile(encrypted.address, { signal: controller.signal });
    setTimeout(() => controller.abort(reason), 100);
    await assert.rejects(pending, error => error === reason);
    assert(Date.now() - started < 5_000, `abort took ${Date.now() - started} ms`);
    rtc.stores[0].set(missing.address, missing.content);
    const legacy = await client.downloadPublicFile(encrypted.address, 1.5);
    assert.deepEqual(legacy.content, original);
    assert.deepEqual((await client.downloadPublicFile(encrypted.address, "2")).content, original);
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
  const { rtc, client } = fixture(encrypted.records);
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
    await entered;
    // Let fetching run as far as the buffer allows, then check that it stopped.
    let settled = gets(rtc);
    for (let previous = -1; previous !== settled; settled = gets(rtc)) {
      previous = settled;
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    const buffered = settled - before;
    assert(buffered > 1, "fetching continues behind a pending write");
    assert(buffered <= Math.floor(32 * 1024 * 1024 / 4_190_208), `${buffered} chunks fetched behind one blocked write`);
    unblock();
    const result = await completion;
    assert.equal(result.bytesWritten, original.length);
    assert.deepEqual(Buffer.concat(chunks), Buffer.from(original));
    assert(chunks.every(chunk => chunk.length <= 4 * 1024 * 1024));
  } finally { reader?.close(); client.close(); }
});

test("invalid pipeTo calls abort the destination they were given", async () => {
  const { client } = fixture();
  try {
    const reader = await client.openPublicFile(large.address);
    for (const [options, expected, close] of [
      [{ end: large.size + 1 }, /outside the file/],
      [{ start: 10, end: 5 }, /outside the file/],
      [{ start: -1 }, /safe integer/],
      [{ size: 1 }, /unknown option `size`/],
      [{ onProgress: "no" }, /onProgress must be a function/],
      [{ signal: {} }, /AbortSignal/],
      [{}, /closed/, true],
    ]) {
      if (close) reader.close();
      let aborted, closed = false;
      const sink = new WritableStream({ write() { assert.fail("nothing is written"); }, close() { closed = true; }, abort(reason) { aborted = reason; } });
      await assert.rejects(reader.pipeTo(sink, options), expected);
      assert.match(String(aborted), expected);
      assert.equal(closed, false);
      assert.equal(sink.locked, false);
    }
  } finally { client.close(); }
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

test("closing a reader from the final progress callback keeps the completed file", async () => {
  const original = new TextEncoder().encode("Final progress callback.".repeat(200));
  const encrypted = encryptPublicFile(original);
  const { client } = fixture(encrypted.records);
  try {
    const reader = await client.openPublicFile(encrypted.address);
    const chunks = [];
    let closed = false, aborted = false;
    const sink = new WritableStream({ write(bytes) { chunks.push(bytes); }, close() { closed = true; }, abort() { aborted = true; } });
    const progress = [];
    const result = await reader.pipeTo(sink, { onProgress(written, total) {
      progress.push([written, total]);
      if (written === total) reader.close();
    } });
    assert.equal(result.bytesWritten, original.length);
    assert.deepEqual(progress.at(-1), [original.length, original.length]);
    assert(closed);
    assert(!aborted);
    assert.deepEqual(Buffer.concat(chunks), Buffer.from(original));
  } finally { client.close(); }
});

test("cancelling during a missing-record retry does not wait out the retry rounds", async () => {
  const original = new TextEncoder().encode("Retry cancellation.".repeat(300));
  const encrypted = encryptPublicFile(original);
  const missing = encrypted.records.find(record => record.address !== encrypted.address);
  const { client } = fixture(encrypted.records.filter(record => record !== missing));
  try {
    for (const cancel of ["close", "signal"]) {
      const reader = await client.openPublicFile(encrypted.address);
      const controller = new AbortController();
      const started = Date.now();
      const pending = cancel === "close"
        ? reader.readRange(0, original.length)
        : reader.pipeTo(new WritableStream(), { signal: controller.signal });
      setTimeout(() => cancel === "close" ? reader.close() : controller.abort(new Error("stop")), 100);
      await assert.rejects(pending, cancel === "close" ? /closed/ : /stop/);
      assert(Date.now() - started < 5_000, `${cancel} took ${Date.now() - started} ms`);
      reader.close();
    }
  } finally { client.close(); }
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

test("closing one reader keeps the records another reader of the file uses", async () => {
  const original = new TextEncoder().encode("Two readers, one file.".repeat(300));
  const encrypted = encryptPublicFile(original);
  const { rtc, client } = fixture(encrypted.records);
  try {
    const first = await client.openPublicFile(encrypted.address);
    const second = await client.openPublicFile(encrypted.address);
    assert.deepEqual(await second.readRange(0, original.length), original);
    first.close();
    const cached = gets(rtc);
    assert.deepEqual(await second.readRange(0, original.length), original);
    assert.equal(gets(rtc), cached, "the open reader's records stay cached");
    second.close();
    const third = await client.openPublicFile(encrypted.address);
    const reopened = gets(rtc);
    assert.deepEqual(await third.readRange(0, original.length), original);
    assert(gets(rtc) > reopened, "the last reader's close released the file's records");
    third.close();
  } finally { client.close(); }
});

test("writer rejection and reader cancellation stop a stream and release its lock", async () => {
  const { client } = fixture();
  try {
    for (const cancel of [false, true]) {
      const reader = await client.openPublicFile(large.address);
      let writes = 0, closed = false, aborted = false;
      const failure = new Error("disk full");
      const sink = new WritableStream({
        write() { writes++; if (cancel) reader.close(); else throw failure; },
        close() { closed = true; },
        abort() { aborted = true; },
      });
      try {
        await assert.rejects(reader.pipeTo(sink, { start: 2 ** 32 }), cancel ? /closed/ : error => error === failure);
        assert.equal(writes, 1);
        assert.equal(closed, false);
        if (cancel) assert(aborted);
        assert.equal(sink.locked, false);
      } finally { reader.close(); }
    }
  } finally { client.close(); }
});

test("in-memory downloads use one JS output buffer and return recoverable allocation failures", async () => {
  const original = new TextEncoder().encode("In-memory download round trip.".repeat(100));
  const encrypted = encryptPublicFile(original);
  const { client } = fixture(encrypted.records);
  const NativeUint8Array = globalThis.Uint8Array;
  let reader;
  try {
    const messages = [];
    const result = await client.downloadPublicFile(encrypted.address,
      { concurrency: 2, maxMemoryBytes: original.length, onProgress: message => messages.push(message) });
    assert.deepEqual(result.content, original);
    assert.equal(result.file.chunks.length, 3);
    // Probes time the first chunks by these messages.
    const chunkProgress = messages.filter(message => message.startsWith("Downloaded chunk "));
    assert.deepEqual(chunkProgress, ["Downloaded chunk 0/3", "Downloaded chunk 1/3", "Downloaded chunk 2/3", "Downloaded chunk 3/3"]);
    const legacy = await client.downloadPublicFile(encrypted.address, 2, message => messages.push(message));
    assert.equal(legacy.hash, result.hash);
    globalThis.Uint8Array = class extends NativeUint8Array {
      constructor(...args) {
        if (args.length === 1 && args[0] === original.length) throw new RangeError("allocation refused");
        super(...args);
      }
    };
    await assert.rejects(client.downloadPublicFile(encrypted.address), /cannot allocate download buffer.*pipeTo/);
    globalThis.Uint8Array = NativeUint8Array;
    reader = await client.openPublicFile(encrypted.address);
    const chunks = [];
    const streamed = await reader.pipeTo(new WritableStream({ write(bytes) { chunks.push(bytes); } }));
    assert.equal(streamed.hash, result.hash);
    assert.deepEqual(Buffer.concat(chunks), Buffer.from(original));
  } finally { globalThis.Uint8Array = NativeUint8Array; reader?.close(); client.close(); }
});
