// Usage: PLAYWRIGHT=<path> node run.mjs <chunks.txt> <out.jsonl> <seed> [sessions] [perSession] [strategies]
import { spawn } from "node:child_process";
import { appendFile, readFile } from "node:fs/promises";
const dir = new URL(".", import.meta.url).pathname;
const [chunksPath, outPath, seedArg, sessionsArg, perArg, stratArg] = process.argv.slice(2);
const PORT = Number(process.env.PORT ?? 5197);
const WARMUP_MS = Number(process.env.WARMUP_MS ?? 5000);
const strategies = (stratArg ?? "baseline,combined").split(",");
let seed = Number(seedArg ?? 1);
const rand = () => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed / 2147483648; };
const shuffle = (xs) => { for (let i = xs.length - 1; i > 0; i--) { const j = Math.floor(rand() * (i + 1)); [xs[i], xs[j]] = [xs[j], xs[i]]; } return xs; };
const chunks = shuffle((await readFile(chunksPath, "utf8")).split("\n").map((s) => s.trim()).filter(Boolean));
const sessions = Number(sessionsArg ?? 5);
const perSession = Number(perArg ?? Math.floor(chunks.length / sessions));
const pw = await import(process.env.PLAYWRIGHT ?? "playwright");
const server = spawn("python3", ["-m", "http.server", String(PORT), "--bind", "127.0.0.1"], { cwd: dir, stdio: "ignore" });
await new Promise((r) => setTimeout(r, 800));
const browser = await pw.chromium.launch({ headless: true });
let seq = 0;
try {
  for (let s = 0; s < sessions; s++) {
    const reads = [];
    let block = [];
    for (const address of chunks.slice(s * perSession, (s + 1) * perSession)) {
      if (block.length === 0) block = shuffle([...strategies]);
      reads.push({ address, strategy: block.pop(), seq: seq++ });
    }
    const context = await browser.newContext();
    const page = await context.newPage();
    page.on("console", (m) => { if (m.type() === "error") console.error("page:", m.text().slice(0, 200)); });
    await page.goto(`http://127.0.0.1:${PORT}/bench.html`);
    await page.waitForFunction(() => window.ready, {}, { timeout: 30_000 });
    const timer = setInterval(async () => { try { console.log(`  session ${s}: ${await page.evaluate(() => window.progress ?? 0)}/${reads.length}`); } catch {} }, 60_000);
    const data = await page.evaluate(([r, w]) => window.bench(r, w), [reads, WARMUP_MS]);
    clearInterval(timer);
    await context.close();
    for (const line of data.reads) await appendFile(outPath, `${JSON.stringify({ session: s, connect_ms: data.connectMs, ...line })}\n`);
    const ok = data.reads.filter((r) => !r.timed_out);
    console.log(`${new Date().toISOString()} session ${s}: connect ${data.connectMs}ms dials ok ${data.dialsOk} failed ${data.dialsFailed}; ` +
      strategies.map((st) => { const ms = ok.filter((r) => r.strategy === st).map((r) => r.trace.total_ms).sort((a, b) => a - b); return `${st} n=${ms.length} p50=${ms[Math.floor(ms.length / 2)]}`; }).join(", "));
  }
} finally {
  await browser.close();
  server.kill();
}
