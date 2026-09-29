// Run the upload quote bench for each variant, interleaved, in fresh browser pages.
import { spawn } from "node:child_process";
import { appendFile } from "node:fs/promises";
const dir = new URL(".", import.meta.url).pathname;
const PORT = Number(process.env.PORT ?? 5196);
const VARIANTS = (process.env.VARIANTS ?? "pkg-v0,pkg-v3").split(",");
const ROUNDS = Number(process.env.ROUNDS ?? 3);
const UPLOADS = Number(process.env.UPLOADS ?? 4);
const SIZE = Number(process.env.SIZE ?? 8 * 1024 * 1024);
const OUT = `${dir}/uploads-${Date.now()}.jsonl`;
const pw = await import(process.env.PLAYWRIGHT ?? "@playwright/test");
const server = spawn("python3", ["-m", "http.server", String(PORT), "--bind", "127.0.0.1"], { cwd: dir, stdio: "ignore" });
await new Promise((r) => setTimeout(r, 800));
const browser = await pw.chromium.launch({ headless: true });
try {
  for (let round = 0; round < ROUNDS; round++) {
    for (const pkg of VARIANTS) {
      const context = await browser.newContext();
      const page = await context.newPage();
      await page.goto(`http://127.0.0.1:${PORT}/uploads.html?pkg=${pkg}`);
      await page.waitForFunction(() => window.ready, {}, { timeout: 30_000 });
      const data = await page.evaluate(([count, size]) => window.bench(count, size), [UPLOADS, SIZE]);
      await context.close();
      await appendFile(OUT, `${JSON.stringify({ pkg, round, ...data })}\n`);
      const ms = data.uploads.filter((u) => u.quoteMs !== undefined).map((u) => u.quoteMs).sort((a, b) => a - b);
      console.log(`${new Date().toISOString()} round ${round} ${pkg}: quoted ${ms.length}/${data.uploads.length}, quote ms ${ms.join(",")}, quotes ${data.uploads.map((u) => u.quotes).join(",")}, errors ${JSON.stringify(data.uploads.filter((u) => u.error).map((u) => u.error))}`);
    }
  }
} finally {
  await browser.close();
  server.kill();
  console.log(`results: ${OUT}`);
}
