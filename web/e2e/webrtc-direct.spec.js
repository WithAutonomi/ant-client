import { expect, test } from "@playwright/test";

test("a URL endpoint skips the local manifest and connects directly", async ({
  page,
  request,
}) => {
  const applicationErrors = [];
  page.on("console", (message) => {
    if (message.type() === "error") applicationErrors.push(message.text());
  });
  const manifestResponse = await request.get(
    "http://127.0.0.1:35000/api/browser-manifest.json",
  );
  expect(manifestResponse.ok()).toBe(true);
  const manifest = await manifestResponse.json();
  const endpoint = manifest.endpoints[0].multiaddr;

  await page.goto(`/?endpoint=${encodeURIComponent(endpoint)}`);
  await expect(page.locator("#manifest-state")).toHaveText(
    "Manual endpoint · manifest skipped",
  );
  await page
    .getByRole("button", { name: "Connect and use as bootstrap", exact: true })
    .click();
  await expect(page.locator("#connection-state")).toContainText("Connected");
  await expect(page.locator("#manifest-state")).toHaveText(
    "Manual bootstrap · 1 direct node",
  );
  await expect(page.locator("#log")).toContainText("HELLO");
  expect(applicationErrors).toEqual([]);
});

test("bootstraps from one address, then uploads and downloads over WebRTC Direct", async ({
  page,
  request,
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

  const manifestResponse = await request.get(
    "http://127.0.0.1:35000/api/browser-manifest.json",
  );
  expect(manifestResponse.ok()).toBe(true);
  const manifest = await manifestResponse.json();
  const endpoint = manifest.endpoints[0].multiaddr;
  await page.goto(`/?endpoint=${encodeURIComponent(endpoint)}`);
  await expect(page.locator("#manifest-state")).toHaveText(
    "Manual endpoint · manifest skipped",
  );
  await expect(page.locator("#endpoint-multiaddr")).toHaveValue(
    /\/webrtc-direct\/certhash\/.+\/p2p\//,
  );

  await page
    .getByRole("button", { name: "Connect and use as bootstrap", exact: true })
    .click();

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
    mimeType: "video/mp4",
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

  await page.getByRole("button", { name: "Prepare video stream" }).click();
  await expect(page.locator("#stream-state")).toContainText("Ready", {
    timeout: 60_000,
  });
  const streamUrl = await page.locator("#stream-video").getAttribute("src");
  const rangeStart = 1_000_000;
  const rangeLength = 8192;
  const streamed = await page.evaluate(
    async ({ streamUrl, rangeStart, rangeLength }) => {
      const response = await fetch(streamUrl, {
        headers: { Range: `bytes=${rangeStart}-${rangeStart + rangeLength - 1}` },
      });
      return {
        status: response.status,
        contentRange: response.headers.get("content-range"),
        acceptRanges: response.headers.get("accept-ranges"),
        bytes: Array.from(new Uint8Array(await response.arrayBuffer())),
      };
    },
    { streamUrl, rangeStart, rangeLength },
  );
  expect(streamed.status).toBe(206);
  expect(streamed.contentRange).toBe(
    `bytes ${rangeStart}-${rangeStart + rangeLength - 1}/${content.length}`,
  );
  expect(streamed.acceptRanges).toBe("bytes");
  expect(streamed.bytes).toEqual(
    Array.from(content.subarray(rangeStart, rangeStart + rangeLength)),
  );
  const tailLength = 512;
  const streamedTail = await page.evaluate(
    async ({ streamUrl, tailLength }) => {
      const response = await fetch(streamUrl, {
        headers: { Range: `bytes=-${tailLength}` },
      });
      return {
        status: response.status,
        contentRange: response.headers.get("content-range"),
        bytes: Array.from(new Uint8Array(await response.arrayBuffer())),
      };
    },
    { streamUrl, tailLength },
  );
  expect(streamedTail.status).toBe(206);
  expect(streamedTail.contentRange).toBe(
    `bytes ${content.length - tailLength}-${content.length - 1}/${content.length}`,
  );
  expect(streamedTail.bytes).toEqual(
    Array.from(content.subarray(content.length - tailLength)),
  );

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
