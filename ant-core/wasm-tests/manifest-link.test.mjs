import assert from "node:assert/strict";
import test from "node:test";
import { decodeManifest, parseManifestLink } from "./pkg/ant_core.js";

// A manifest built by `ant manifest create --embed-public ADDRESS=file.bin`
// against mainnet: one embedded entry whose DataMap is a shrunk child map,
// in the v1 layout (chunk records as one `bin`, 411 link characters).
const LINK =
  "ant://manifest/wUFOVAGCpG5hbWWtbWFpbm5ldC1iZW5jaKdlbnRyaWVzkYKkcGF0aKhmaWxlLmJpbqZzb3VyY2WBqEVtY" +
  "mVkZGVkgahkYXRhX21hcIKmY2h1bmtzxMxpRDt_7LwtBIlaUVXYSWWmtvExwFnaw-EI8BT00k2OeAyP9c_3dHKMJq3CcHm5m" +
  "bxhu3qKkzaki2XUpqYzTQduAAAPUxh69f4_acErQX6MpRtt6kljlk5TBCCJtVNtnn6MHn2EfAZmDWcSgMWVUpWdZ_Lz7QMaR" +
  "r7QL8HgLseYAy6z6acAAA9Tf0s-g-0LQ0Zo3tvNmAI0dOlovPC1iUNrL7ucFRXivzMdbXM2wOG9D4kZPFilIaxdj2Haac8s4" +
  "Uq4D7uZJrgdDgAAD1SlY2hpbGQB";
const ADDRESS = "134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889";
const MAGIC = [0xc1, 0x41, 0x4e, 0x54];

test("a manifest link decodes to its entries with embedded DataMap bytes", () => {
  const parsed = parseManifestLink(LINK);
  assert.equal(parsed.kind, "manifest");
  assert.equal(parsed.manifest.name, "mainnet-bench");
  assert.equal(parsed.manifest.entries.length, 1);
  const entry = parsed.manifest.entries[0];
  assert.equal(entry.kind, "embedded");
  assert.equal(entry.name, "file.bin");
  assert.equal(entry.path, "file.bin");
  assert.equal(entry.size, undefined);
  assert.equal(entry.knownSize, undefined, "a shrunk map does not reveal the file size");
  assert.equal(entry.address, ADDRESS);
  assert.ok(entry.dataMap instanceof Uint8Array);
  assert.equal(entry.dataMap.byteLength, 320);
});

test("a file link and a bare address both parse as a file", () => {
  assert.deepEqual(parseManifestLink(`ant://${ADDRESS}`), { kind: "file", address: ADDRESS });
  assert.deepEqual(parseManifestLink(ADDRESS), { kind: "file", address: ADDRESS });
  assert.throws(() => parseManifestLink(`ant://${ADDRESS}?dn=x`));
  assert.throws(() => parseManifestLink("ant://manifest/!!!"));
});

test("decodeManifest requires the full header", () => {
  const payload = LINK.slice("ant://manifest/".length);
  const bytes = Uint8Array.from(Buffer.from(payload, "base64url"));
  assert.deepEqual([...bytes.subarray(0, 4)], MAGIC);
  const decoded = decodeManifest(bytes);
  assert.equal(decoded.entries[0].address, ADDRESS);
  assert.throws(() => decodeManifest(bytes.subarray(1)));
  const wrongVersion = Uint8Array.from(bytes);
  wrongVersion[4] = 2;
  assert.throws(() => decodeManifest(wrongVersion), /version/);
});
