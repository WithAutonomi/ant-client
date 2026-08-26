import { expect, test } from "@playwright/test";

test("uploads and downloads a nested-DataMap file over WebRTC Direct", async ({
  page,
}) => {
  test.setTimeout(180_000);
  const applicationErrors = [];
  page.on("console", (message) => {
    if (message.type() === "error") applicationErrors.push(message.text());
  });
  await page.addInitScript(() => {
    Object.defineProperty(globalThis, "showSaveFilePicker", {
      value: undefined,
      configurable: true,
    });
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

  const maxChunkSize = 4_190_208;
  const content = Buffer.allocUnsafe(3 * maxChunkSize + 1);
  let state = 0x9e3779b9;
  for (let index = 0; index < content.length; index += 1) {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    content[index] = state;
  }
  await page.locator("#upload-file-input").setInputFiles({
    name: "nested-datamap.bin",
    mimeType: "application/octet-stream",
    buffer: content,
  });
  await page
    .locator("#wallet-secret")
    .fill("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
  await page.getByRole("button", { name: "Pay and upload file" }).click();

  try {
    await expect(page.locator("#upload-state")).toContainText("Uploaded", {
      timeout: 150_000,
    });
  } catch (error) {
    const protocolLog = await page.locator("#log").textContent();
    throw new Error(`${error.message}\n\nBrowser protocol log:\n${protocolLog}`);
  }
  await expect(page.locator("#upload-result-records")).toContainText("8 records");

  const downloadStarted = page.waitForEvent("download");
  await page.getByRole("button", { name: "Download and save file" }).click();
  try {
    await expect(page.locator("#download-state")).toContainText(
      "Browser download started",
      { timeout: 120_000 },
    );
  } catch (error) {
    const protocolLog = await page.locator("#log").textContent();
    throw new Error(`${error.message}\n\nBrowser protocol log:\n${protocolLog}`);
  }
  const download = await downloadStarted;
  expect(download.suggestedFilename()).toBe("nested-datamap.bin");
  expect(applicationErrors).toEqual([]);
});
