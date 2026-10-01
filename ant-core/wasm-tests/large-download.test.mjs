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
        const gets = rtc.requests.filter(r => r.method === "get_chunk").length;
        for (const value of [-1, 0.5, NaN, Infinity, 2 ** 53]) {
          await assert.rejects(reader.readRange(value, 1), /safe integer/);
          await assert.rejects(reader.readRange(0, value), /safe integer/);
        }
        await assert.rejects(reader.readRange(0, 2 ** 32 + 1), /range reads are limited/);
        assert.equal(rtc.requests.filter(r => r.method === "get_chunk").length, gets);
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
  const { client } = fixture();
  try {
    await assert.rejects(client.downloadPublicFile(large.address, undefined, undefined, 64 * 1024 * 1024), /memory budget.*pipeTo/);
    await assert.rejects(client.downloadPrivateFile({ data_map: map.content }, undefined, undefined, 64 * 1024 * 1024), /memory budget.*pipeTo/);
    for (const value of [-1, 1.5, Infinity, NaN, 2 ** 53]) {
      await assert.rejects(client.downloadPublicFile(large.address, undefined, undefined, value), /safe integer/);
    }
  } finally { client.close(); }
});

test("pipeTo writes ranges beyond 4 GiB to disk and honors write backpressure", async () => {
  const { rtc, client } = fixture();
  const directory = await mkdtemp(join(tmpdir(), "ant-wasm-download-"));
  let reader;
  try {
    reader = await client.openPublicFile(large.address);
    let unblock, firstWrite;
    const entered = new Promise(resolve => { firstWrite = resolve; });
    const gate = new Promise(resolve => { unblock = resolve; });
    const path = join(directory, "tail.bin");
    const disk = Writable.toWeb(createWriteStream(path));
    const writer = disk.getWriter();
    let writes = 0;
    const sink = new WritableStream({
      async write(bytes) {
        if (++writes === 1) { firstWrite(); await gate; }
        await writer.write(bytes);
      },
      async close() { await writer.close(); writer.releaseLock(); },
      async abort(error) { await writer.abort(error); writer.releaseLock(); },
    });
    const start = 2 ** 32 - 8;
    const completion = reader.pipeTo(sink, { start });
    await entered;
    const gets = rtc.requests.filter(r => r.method === "get_chunk").length;
    await new Promise(resolve => setTimeout(resolve, 30));
    assert.equal(rtc.requests.filter(r => r.method === "get_chunk").length, gets);
    unblock();
    const result = await completion;
    assert.equal(result.bytesWritten, large.size - start);
    assert.deepEqual(await readFile(path), Buffer.alloc(large.size - start, large.byte));
    assert.equal(sink.locked, false);
  } finally { reader?.close(); client.close(); await rm(directory, { recursive: true, force: true }); }
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
    const result = await client.downloadPublicFile(encrypted.address, 2, undefined, original.length);
    assert.deepEqual(result.content, original);
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
