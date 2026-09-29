// Stream one mainnet address through the SDK all-in-one example with the diagnostic WASM,
// recording service-worker range timings, per-peer chunk GETs, lookups, and retry rounds.
import { spawn } from "node:child_process";

const SDK = process.env.SDK_DIR ?? "../ant-client-browser-sdk";
const ADDRESS = process.env.ADDRESS ?? "134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889";
const PORT = Number(process.env.PORT ?? 5195);
const WATCH_SECONDS = Number(process.env.WATCH_SECONDS ?? 120);
const pw = await import(process.env.PLAYWRIGHT ?? "@playwright/test");

const server = spawn(process.execPath, [`${SDK}/node_modules/vite/bin/vite.js`, "--config", "examples/all-in-one/vite.config.ts",
  "--port", String(PORT), "--strictPort"], { cwd: SDK, stdio: "ignore" });
for (let i = 0; i < 150; i++) {
  try { if ((await fetch(`http://127.0.0.1:${PORT}/`)).ok) break; } catch {}
  await new Promise((r) => setTimeout(r, 200));
}
const t0 = Date.now();
const at = () => ((Date.now() - t0) / 1000).toFixed(1);
let browser;
try {
  browser = await pw.chromium.launch({ headless: true });
  const page = await browser.newPage();
  page.on("console", (m) => {
    const text = m.text();
    if (m.type() === "error" || text.startsWith("[probe]") || text.startsWith("[diag]")) console.log(`${at()} ${text}`);
  });
  page.on("pageerror", (e) => console.log(`${at()} pageerror ${e.message}`));
  await page.addInitScript(() => {
    const t0 = performance.now();
    const at = () => ((performance.now() - t0) / 1000).toFixed(1);
    navigator.serviceWorker?.addEventListener("message", (event) => {
      const d = event.data;
      if (d?.type !== "autonomi-file-range") return;
      const started = performance.now();
      const port = event.ports[0];
      const post = port.postMessage.bind(port);
      port.postMessage = (message, transfer) => {
        console.log(`[probe] range ${d.start}+${d.length} -> ${message.ok ? "ok" : `ERROR ${message.error}`} in ${Math.round(performance.now() - started)}ms`);
        return post(message, transfer);
      };
      console.log(`[probe] range request ${d.start}+${d.length}`);
    });
  });
  await page.goto(`http://127.0.0.1:${PORT}/`);
  await page.locator("#connect").click();
  await page.waitForFunction(() => document.querySelector("#connection").value.startsWith("Connected"), {}, { timeout: 120_000 });
  console.log(`${at()} connected`);
  await page.locator("#address").fill(ADDRESS);
  await page.locator("#stream").click();
  await page.waitForFunction(() => document.querySelector("#media")?.src, {}, { timeout: 180_000 });
  await page.evaluate(() => {
    const v = document.querySelector("#media");
    v.muted = true;
    void v.play().catch(() => {});
    for (const name of ["playing", "waiting", "stalled", "error"])
      v.addEventListener(name, () => console.log(`[probe] video ${name} t=${v.currentTime.toFixed(2)}${v.error ? ` err=${v.error.message}` : ""}`));
  });
  const SEEK_AFTER = Number(process.env.SEEK_AFTER ?? 0);
  const SEEK_TO = Number(process.env.SEEK_TO ?? 0);
  for (let s = 0; s < WATCH_SECONDS; s += 10) {
    await page.waitForTimeout(10_000);
    if (SEEK_AFTER && s + 10 === SEEK_AFTER) {
      console.log(`${at()} seeking to ${SEEK_TO}s`);
      await page.evaluate((to) => {
        const v = document.querySelector("#media");
        v.addEventListener("seeked", () => console.log(`[probe] video seeked t=${v.currentTime.toFixed(2)}`), { once: true });
        v.currentTime = to;
      }, SEEK_TO);
    }
    console.log(`${at()} state ${await page.evaluate(() => {
      const v = document.querySelector("#media");
      const b = [...Array(v.buffered.length).keys()].map((i) => `${v.buffered.start(i).toFixed(0)}-${v.buffered.end(i).toFixed(0)}`).join(",");
      return `t=${v.currentTime.toFixed(1)} rs=${v.readyState} buffered=[${b}]`;
    })}`);
  }
} finally {
  await browser?.close();
  server.kill("SIGINT");
}
