// Time mainnet uploads up to the payment request, then decline to pay. This covers
// the upload's lookups (existence checks, witnessed close groups) and quote collection.
// Nothing is paid for or stored.
const pkg = new URLSearchParams(location.search).get("pkg");
const { default: init, BrowserNetworkClient, setBrowserTrace, mainnetNetworkDefaults } = await import(`./${pkg}/ant_core.js`);
const RANDOM_CHUNK_BYTES = 65536;
const UPLOAD_TIMEOUT_MS = 300_000;
const STOP = "benchmark: payment declined before any transaction";
function randomBytes(length) {
  const bytes = new Uint8Array(length);
  for (let offset = 0; offset < length; offset += RANDOM_CHUNK_BYTES)
    crypto.getRandomValues(bytes.subarray(offset, Math.min(length, offset + RANDOM_CHUNK_BYTES)));
  return bytes;
}
window.bench = async (count, size) => {
  await init();
  const dials = [];
  setBrowserTrace((json) => {
    const t = JSON.parse(json);
    if (t.operation === "connect" && t.phase !== "start") dials.push({ ms: Math.round(t.elapsed_ms), ok: t.detail === "connected" });
  });
  const defaults = mainnetNetworkDefaults();
  const client = new BrowserNetworkClient(defaults.seeds.map((multiaddr) => ({ multiaddr })));
  const uploads = [];
  for (let i = 0; i < count; i++) {
    const content = randomBytes(size);
    const started = performance.now();
    let quoteMs, quotes;
    const pay = async (_request, offered) => {
      quoteMs = Math.round(performance.now() - started);
      quotes = offered?.length;
      throw new Error(STOP);
    };
    let error, timer;
    try {
      await Promise.race([
        client.uploadPublicFile(content, `bench-${i}.bin`, "application/octet-stream", defaults.payment, pay,
          undefined, undefined, async () => {}, "single"),
        new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("timeout")), UPLOAD_TIMEOUT_MS); }),
      ]);
    } catch (caught) {
      error = String(caught);
    } finally { clearTimeout(timer); }
    uploads.push({ i, quoteMs, quotes, totalMs: Math.round(performance.now() - started),
      stoppedAtPayment: !!error?.includes(STOP) || quoteMs !== undefined, error: error?.includes(STOP) ? undefined : error?.slice(0, 200) });
  }
  client.close();
  return { uploads, dials };
};
window.ready = true;
