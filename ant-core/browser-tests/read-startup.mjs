// Read-only live-network probe. Build WASM with browser-wasm,test-utils, then:
// node read-startup.mjs ../wasm-tests/pkg seeds.json [single|all] [runs] [core|sdk]
// Emits JSONL. Each fresh context stops after three content chunks or 180 s.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, join } from 'node:path';

const [pkgArg, seedsArg, mode = 'single', runsArg = '1', lifecycle = 'core'] = process.argv.slice(2);
if (!pkgArg || !seedsArg) throw new Error('usage: read-startup.mjs <WASM package> <seeds.json> [single|all] [runs]');
const pkg = resolve(pkgArg);
const seeds = JSON.parse(await readFile(seedsArg, 'utf8'));
const selected = mode === 'single' ? seeds.slice(0, 1) : seeds;
const server = createServer(async (req, res) => {
  const name = req.url?.split('?')[0];
  if (name === '/') { res.setHeader('Content-Type', 'text/html'); res.end('<!doctype html><title>Read startup trace</title>'); return; }
  if (!['/ant_core.js', '/ant_core_bg.wasm'].includes(name)) { res.writeHead(404).end(); return; }
  try { const bytes = await readFile(join(pkg, name.slice(1))); res.setHeader('Content-Type', name.endsWith('.wasm') ? 'application/wasm' : 'text/javascript'); res.end(bytes); }
  catch (error) { res.writeHead(500).end(String(error)); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({headless: true});
try {
  for (let run = 1; run <= Number(runsArg); run++) {
    const context = await browser.newContext();
    const page = await context.newPage();
    let finish;
    const done = new Promise(resolve => { finish = resolve; });
    await page.exposeFunction('recordTrace', event => {
      console.log(JSON.stringify({run, mode, ...event}));
      if (event.kind === 'error' || event.message?.startsWith('Downloaded chunk 3/')) finish();
    });
    await page.goto(`http://127.0.0.1:${server.address().port}/`);
    const running = page.evaluate(async ({seeds, lifecycle}) => {
      const core = await import('/ant_core.js'); await core.default();
      const start = performance.now(); let downloadStart;
      const record = event => globalThis.recordTrace({ms: performance.now() - start,
        download_ms: downloadStart === undefined ? undefined : performance.now() - downloadStart, ...event});
      core.setBrowserTrace(value => record({kind: 'trace', ...JSON.parse(value)}));
      const client = new core.BrowserNetworkClient(seeds);
      // SDK mode authenticates through the retained network pool before the
      // download; core mode starts with a completely cold network client.
      if (lifecycle === 'sdk') {
        await client.connect();
      }
      record({kind: 'connected'});
      try {
        downloadStart = performance.now();
        await client.downloadPublicFile('134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889', undefined,
          message => record({kind: 'progress', message}));
      } catch(error) { record({kind: 'error', message: String(error)}); }
      finally { client.close(); }
    }, {seeds: selected, lifecycle}).catch(error => { if (!page.isClosed()) console.log(JSON.stringify({run, kind: 'error', message: String(error)})); finish(); });
    const timer = setTimeout(() => { console.log(JSON.stringify({run, kind: 'limit', seconds: 180})); finish(); }, 180_000);
    await done; clearTimeout(timer); await context.close(); await running;
  }
} finally { await browser.close(); server.close(); }
