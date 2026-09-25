import { chromium } from '../../../ant-core/browser-tests/node_modules/playwright/index.mjs';
import { readFile } from 'node:fs/promises';
import assert from 'node:assert/strict';

const info = await (await fetch('http://127.0.0.1:35000/api/info')).json();
assert.equal(info.evm.network, 'local-anvil');
assert.ok(['127.0.0.1', 'localhost'].includes(new URL(info.evm.rpc_url).hostname));
const manifest = await (await fetch('http://127.0.0.1:35000/api/browser-manifest.json')).json();
const browser = await chromium.launch({headless: true, args: ['--force-fieldtrials=WebRTC-NoSdpMangleUfrag/Enabled/']});
try {
  const page = await browser.newPage({acceptDownloads: true});
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.addInitScript(() => { window.showSaveFilePicker = undefined; });
  await page.goto('http://127.0.0.1:35174');
  await page.fill('#bootstrap', manifest.endpoints[0].multiaddr);
  await page.click('#connect');
  await page.waitForFunction(() => document.querySelector('#connection').textContent.startsWith('Connected'), null, {timeout: 60_000});
  await page.fill('#payment-rpc', info.evm.rpc_url);
  // Public default Anvil account, only after checking the local-devnet guard.
  await page.fill('#wallet', '0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80');
  const content = Buffer.from('Shared native and browser read startup regression.\n'.repeat(256));
  await page.setInputFiles('#upload-input', {name: 'read-startup.txt', mimeType: 'text/plain', buffer: content});
  await page.click('#upload');
  await page.waitForFunction(() => document.querySelector('#address').value.length === 64, null, {timeout: 120_000});
  const downloadPromise = page.waitForEvent('download', {timeout: 120_000});
  await page.click('#download');
  const download = await downloadPromise;
  assert.deepEqual(await readFile(await download.path()), content);
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({result: 'passed', bytes: content.length, matches: true, network: info.evm.network}));
  console.log(await page.locator('#log').textContent());
} finally { await browser.close(); }
