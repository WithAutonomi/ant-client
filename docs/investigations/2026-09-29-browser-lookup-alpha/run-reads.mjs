// Run the cold-read bench for each variant, interleaved, in fresh browser pages.
import { spawn } from "node:child_process";
import { appendFile } from "node:fs/promises";
const dir = new URL(".", import.meta.url).pathname;
const PORT = Number(process.env.PORT ?? 5196);
const VARIANTS = (process.env.VARIANTS ?? "pkg-v0,pkg-v1,pkg-v2").split(",");
const ROUNDS = Number(process.env.ROUNDS ?? 2);
const INDICES = [4, 16, 28, 40, 52, 64, 76, 88, 100, 112, 124, 136];
const OUT = `${dir}/reads-${Date.now()}.jsonl`;
const pw = await import(process.env.PLAYWRIGHT ?? "@playwright/test");
const server = spawn("python3", ["-m", "http.server", String(PORT), "--bind", "127.0.0.1"], { cwd: dir, stdio: "ignore" });
await new Promise((r) => setTimeout(r, 800));
const browser = await pw.chromium.launch({ headless: true });
try {
  for (let round = 0; round < ROUNDS; round++) {
    for (const pkg of VARIANTS) {
      const context = await browser.newContext();
      const page = await context.newPage();
      await page.goto(`http://127.0.0.1:${PORT}/reads.html?pkg=${pkg}`);
      await page.waitForFunction(() => window.ready, {}, { timeout: 30_000 });
      const data = await page.evaluate((indices) => window.bench(indices), INDICES);
      await context.close();
      await appendFile(OUT, `${JSON.stringify({ pkg, round, ...data })}\n`);
      const ms = data.reads.map((r) => r.ms).sort((a, b) => a - b);
      console.log(`${new Date().toISOString()} round ${round} ${pkg}: open ${data.openMs}ms, reads p50 ${ms[Math.floor(ms.length / 2)]}ms max ${ms.at(-1)}ms errors ${data.reads.filter((r) => r.error).length}`);
    }
  }
} finally {
  await browser.close();
  server.kill();
  console.log(`results: ${OUT}`);
}
