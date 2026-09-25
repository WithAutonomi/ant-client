import { test, expect } from "@playwright/test";
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
test("real Chromium creates, updates, reads and resolves pointers on real nodes", async ({ page, request }) => {
  const manifest = await (await request.get("http://127.0.0.1:35000/api/browser-manifest.json")).json();
  const info = await (await request.get("http://127.0.0.1:35000/api/info")).json();
  const errors = []; page.on("pageerror", error => errors.push(error.message));
  await page.goto("/");
  await page.waitForFunction(() => typeof globalThis.runPointerIntegration === "function");
  const result = await page.evaluate(options => globalThis.runPointerIntegration(options), {
    endpoint: manifest.endpoints[0], payment: manifest.payment, rpcUrl: info.evm.rpc_url,
  });
  expect(result.payments).toBe(4);
  expect(result.address).toBe(result.expectedAddress);
  expect(result.createdCounter).toBe("0"); expect(result.updatedCounter).toBe("1");
  expect(result.readCounter).toBe("2"); expect(result.readKind).toBe("pointer");
  expect(result.readTarget).toBe(result.endAddress);
  expect(result.resolved).toEqual({ kind: "chunk", kindTag: 0, target: "03".repeat(32) });
  expect(result.absent).toBeNull();
  expect(errors).toEqual([]);
});
