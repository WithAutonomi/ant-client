// Time cold record reads of one mainnet video: each read needs a record not yet fetched,
// so it includes discovery of that record's holders.
const pkg = new URLSearchParams(location.search).get("pkg");
const { default: init, BrowserNetworkClient, setBrowserTrace, mainnetNetworkDefaults } = await import(`./${pkg}/ant_core.js`);
const VIDEO = "134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889";
const SIZE = 611984394;
const RECORDS = 147;
const READ_TIMEOUT_MS = 120_000;
window.bench = async (indices) => {
  await init();
  const t0 = performance.now();
  const dials = [];
  setBrowserTrace((json) => {
    const t = JSON.parse(json);
    if (t.operation === "connect" && t.phase !== "start") dials.push({ ms: Math.round(t.elapsed_ms), ok: t.detail === "connected" });
  });
  const client = new BrowserNetworkClient(mainnetNetworkDefaults().seeds.map((multiaddr) => ({ multiaddr })));
  const reader = await client.openPublicFile(VIDEO);
  const openMs = Math.round(performance.now() - t0);
  const reads = [];
  for (const index of indices) {
    const offset = Math.floor((index + 0.5) * SIZE / RECORDS);
    const started = performance.now();
    let timer;
    try {
      await Promise.race([reader.readRange(offset, 1),
        new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("timeout")), READ_TIMEOUT_MS); })]);
      reads.push({ index, ms: Math.round(performance.now() - started) });
    } catch (error) {
      reads.push({ index, ms: Math.round(performance.now() - started), error: String(error) });
    } finally { clearTimeout(timer); }
  }
  reader.close();
  client.close();
  return { openMs, reads, dials };
};
window.ready = true;
