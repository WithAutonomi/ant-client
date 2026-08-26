import { expect, test } from "@playwright/test";

test("authenticates a real ant-node over WebRTC Direct", async ({ page }) => {
  const applicationErrors = [];
  page.on("console", (message) => {
    if (message.type() === "error") applicationErrors.push(message.text());
  });

  const manifestUrl = "http://127.0.0.1:35000/api/browser-manifest.json";
  await page.goto(`/?manifest=${encodeURIComponent(manifestUrl)}`);
  await expect(page.locator("#manifest-state")).toContainText("direct nodes", {
    timeout: 120_000,
  });
  await expect(page.locator("#endpoint-multiaddr")).toHaveValue(
    /\/webrtc-direct\/certhash\/.+\/p2p\//,
  );

  await page.getByRole("button", { name: "Connect", exact: true }).click();

  try {
    await expect(page.locator("#connection-state")).toContainText("Connected");
  } catch (error) {
    const protocolLog = await page.locator("#log").textContent();
    throw new Error(`${error.message}\n\nBrowser protocol log:\n${protocolLog}`);
  }
  await expect(page.locator("#log")).toContainText("HELLO");
  expect(applicationErrors).toEqual([]);
});
