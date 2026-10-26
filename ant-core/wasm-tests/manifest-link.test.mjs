import assert from "node:assert/strict";
import test from "node:test";
import vm from "node:vm";
import {
  createFileLink,
  createManifest,
  decodeManifest,
  parseManifestLink,
} from "./pkg/ant_core.js";

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

test("createManifest takes JavaScript input and writes what the decoders read", () => {
  const embedded = parseManifestLink(LINK).manifest.entries[0];
  const created = createManifest({
    name: "files",
    torrent: { infoHashV1: "ab".repeat(20) },
    entries: [
      { kind: "public", path: "z.txt", size: 20, sha256: "ef".repeat(32), address: `ant://${"cd".repeat(32)}` },
      { kind: "embedded", path: "a.bin", dataMap: embedded.dataMap },
    ],
  });
  assert.ok(created.bytes instanceof Uint8Array);
  assert.deepEqual([...created.bytes.subarray(0, 4)], MAGIC);
  assert.equal(created.linkRecommended, true);
  assert.deepEqual(
    created.manifest.entries.map(({ name, kind }) => [name, kind]),
    [["a.bin", "embedded"], ["z.txt", "public"]],
    "entries are listed in the order the bytes hold them",
  );
  assert.equal(created.manifest.entries[0].address, ADDRESS);
  assert.equal(created.manifest.entries[1].size, 20);
  assert.equal(created.manifest.entries[1].sha256, "ef".repeat(32));
  assert.equal(created.manifest.entries[0].sha256, undefined);
  assert.equal(created.manifest.torrent.infoHashV1, "ab".repeat(20));

  assert.deepEqual(decodeManifest(created.bytes), created.manifest);
  assert.deepEqual(parseManifestLink(created.link), { kind: "manifest", manifest: created.manifest });
});

test("createManifest refuses input it does not know", () => {
  const address = "cd".repeat(32);
  for (const [input, error] of [
    [{ entries: [], extra: true }, /unknown field `extra`/],
    [{ entries: [{ kind: "public", address, extra: true }] }, /unknown field `extra`/],
    [{ entries: [{ kind: "Public", address }] }, /unknown variant `Public`/],
    [{ entries: [{ kind: "embedded", dataMap: [1, 2, 3] }] }, /invalid DataMap/],
    [{ entries: [{ kind: "public", address: LINK }] }, /public entry requires a file address/],
    [{ entries: [{ kind: "public", address, sha256: "ab" }] }, /sha256 must be 32 bytes of hex/],
    [{ torrent: { infoHashV1: "ab".repeat(20), infoHashV3: "" }, entries: [] }, /unknown field `infoHashV3`/],
    [{ entries: [{ kind: "public", address, size: 2 ** 53 }] }, /expected u64/],
    [{ entries: [{ kind: "public", address, size: 2n ** 53n }] }, /safe integer/],
  ]) {
    assert.throws(() => createManifest(input), error);
  }
  // Whatever the object's prototype or realm.
  for (const input of [
    Object.assign(Object.create(null), { entries: [], extra: true }),
    { entries: [], torrent: Object.assign(Object.create(null), { infoHashV1: "ab".repeat(20), extra: true }) },
    vm.runInNewContext("({ entries: [], extra: true })"),
  ]) {
    assert.throws(() => createManifest(input), /unknown field `extra`/);
  }
});

test("createFileLink formats a public address in the shared link grammar", () => {
  assert.equal(createFileLink(ADDRESS), `ant://${ADDRESS}`);
  assert.equal(createFileLink(`ant://${ADDRESS.toUpperCase()}`), `ant://${ADDRESS}`);
  assert.throws(() => createFileLink(LINK), /expected a public file address/);
  assert.throws(() => createFileLink("not an address"));
});
