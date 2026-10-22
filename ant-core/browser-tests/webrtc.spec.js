import { test, expect } from "@playwright/test";
import { writeFile } from "node:fs/promises";
test("real Chromium authenticates, resumes one paid upload, and downloads by address", async ({ page, request }) => {
  const manifest = await (await request.get("http://127.0.0.1:35000/api/browser-manifest.json")).json();
  const info = await (await request.get("http://127.0.0.1:35000/api/info")).json();
  expect(info.evm.network).toBe("local-anvil");
  const errors = []; page.on("pageerror", error => errors.push(error.message));
  await page.goto("/");
  await page.waitForFunction(() => typeof globalThis.runIntegration === "function");
  const result = await page.evaluate(options => globalThis.runIntegration(options), {
    endpoint: manifest.endpoints[0], payment: manifest.payment, rpcUrl: info.evm.rpc_url,
  });
  expect(result.hello).toBe("hello"); expect(result.payments).toBe(1);
  expect(result.size).toBe(result.expectedSize); expect(result.matches).toBe(true);
  expect(result.records).toBeGreaterThan(1); expect(result.replicas).toBeGreaterThanOrEqual(4);
  expect(errors).toEqual([]);
});
test("real Chromium creates, updates, reads, resolves and hands over pointers on real nodes", async ({ page, request }) => {
  const manifest = await (await request.get("http://127.0.0.1:35000/api/browser-manifest.json")).json();
  const info = await (await request.get("http://127.0.0.1:35000/api/info")).json();
  const errors = []; page.on("pageerror", error => errors.push(error.message));
  await page.goto("/");
  await page.waitForFunction(() => typeof globalThis.runPointerIntegration === "function");
  const result = await page.evaluate(options => globalThis.runPointerIntegration(options), {
    endpoint: manifest.endpoints[0], payment: manifest.payment, rpcUrl: info.evm.rpc_url,
  });
  expect(result.payments).toBe(5);
  expect(result.address).toBe(result.expectedAddress);
  expect(result.createdCounter).toBe("0"); expect(result.updatedCounter).toBe("1");
  expect(result.readCounter).toBe("2"); expect(result.readKind).toBe("pointer");
  expect(result.readTarget).toBe(result.endAddress);
  expect(result.resolved).toEqual({ kind: "chunk", kindTag: 0, target: "03".repeat(32) });
  expect(result.absent).toBeNull();
  expect(result.transferredCounter).toBe("18446744073709551615");
  expect(result.finalityStatus).toBe("final");
  expect(result.finalityTransferredTo).toBe(result.endAddress);
  expect(errors).toEqual([]);
});
test("real Chromium recovers a pointer payment its page lost, paying once, and builds a manifest", async ({ page, request }, testInfo) => {
  const manifest = await (await request.get("http://127.0.0.1:35000/api/browser-manifest.json")).json();
  const info = await (await request.get("http://127.0.0.1:35000/api/info")).json();
  const errors = []; page.on("pageerror", error => errors.push(error.message));
  await page.goto("/");
  await page.waitForFunction(() => typeof globalThis.runPointerRecoveryIntegration === "function");
  const result = await page.evaluate(options => globalThis.runPointerRecoveryIntegration(options), {
    endpoint: manifest.endpoints[0], payment: manifest.payment, rpcUrl: info.evm.rpc_url,
  });
  expect(result.payments).toBe(1);
  expect(result.lostError).toContain("page lost after the payment confirmed");
  expect(result.lostError).not.toContain("payment was not submitted");
  expect(result.submission.transactionHash).toMatch(/^0x[0-9a-f]{64}$/);
  expect(result.beforeRecovery).toBeNull();
  expect(result.stored.address).toBe(result.expectedAddress); expect(result.stored.counter).toBe("0");
  expect(result.read.counter).toBe("0"); expect(result.read.target).toBe("05".repeat(32));
  expect(result.existsError).toMatch(/already exists[\s\S]*payment was not submitted$/);
  expect(result.manifestEntry).toMatchObject({ name: "docs/readme.txt", kind: "public", address: result.fileAddress, size: 16 });
  expect(result.linkMatches).toBe(true);
  // Kept for decoding with the native CLI: `ant manifest show <file>`.
  await writeFile(testInfo.outputPath("browser-created.ant"), Buffer.from(result.manifestBytes));
  await writeFile(testInfo.outputPath("browser-created-link.txt"), result.link);
  expect(errors).toEqual([]);
});
