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
