import assert from "node:assert/strict";
import test from "node:test";
import { decodeManifest, parseManifestLink } from "./pkg/ant_core.js";

// A manifest built by `ant manifest create --embed-public ADDRESS=file.bin`
// against mainnet: one embedded entry whose DataMap is a shrunk child map.
const LINK =
  "ant://manifest/wUFOVAGCpG5hbWWtbWFpbm5ldC1iZW5jaKdlbnRyaWVzkYOkcGF0aKhmaWxlLmJpbqRzaXplwKZzb3VyY2WBqEVtYmVkZGVkgahkYXRhX21hcIOndmVyc2lvbgGxY2h1bmtfaWRlbnRpZmllcnOThKVpbmRleACoZHN0X2hhc2jcACBpRDt_zOzMvC0EzIlaUVXM2EllzKbMtszxMczAWczazMPM4QjM8BTM9MzSTcyOeKhzcmNfaGFzaNwAIAzMj8z1zM_M93RyzIwmzK3MwnB5zLnMmcy8Ycy7esyKzJM2zKTMi2XM1MymzKYzTQduqHNyY19zaXplzQ9ThKVpbmRleAGoZHN0X2hhc2jcACAYesz1zP4_aczBK0F-zIzMpRttzOpJY8yWTlMEIMyJzLVTbcyefsyMHn3MhKhzcmNfaGFzaNwAIHwGZg1nEsyAzMXMlVLMlcydZ8zyzPPM7QMaRsy-zNAvzMHM4C7Mx8yYAy7Ms8zpzKeoc3JjX3NpemXND1OEpWluZGV4Aqhkc3RfaGFzaNwAIH9LPsyDzO0LQ0ZozN7M28zNzJgCNHTM6WjMvMzwzLXMiUNrL8y7zJwVFczizL8zqHNyY19oYXNo3AAgHW1zNszAzOHMvQ_MiRk8WMylIcysXcyPYczaaczPLMzhSsy4D8y7zJkmzLgdDqhzcmNfc2l6Zc0PVKVjaGlsZAE";
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
