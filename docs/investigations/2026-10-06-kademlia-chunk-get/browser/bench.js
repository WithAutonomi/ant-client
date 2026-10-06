// V2-1358: read chunks with interleaved strategies on one fresh browser client.
const { default: init, BrowserNetworkClient, setBrowserTrace, mainnetNetworkDefaults } = await import("./pkg/ant_core.js");
const READ_TIMEOUT_MS = 180_000;
window.bench = async (reads, warmupMs) => {
  await init();
  const dials = [];
  setBrowserTrace((json) => {
    const t = JSON.parse(json);
    if (t.operation === "connect" && t.phase !== "start") dials.push({ ms: Math.round(t.elapsed_ms), ok: t.detail === "connected" });
  });
  const t0 = performance.now();
  const client = new BrowserNetworkClient(mainnetNetworkDefaults().seeds.map((multiaddr) => ({ multiaddr })));
  await client.connect();
  const connectMs = Math.round(performance.now() - t0);
  await new Promise((r) => setTimeout(r, warmupMs));
  const out = [];
  for (const { address, strategy, seq } of reads) {
    const started = Date.now();
    let timer;
    try {
      const json = await Promise.race([client.benchChunkRead(address, strategy),
        new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("timeout")), READ_TIMEOUT_MS); })]);
      out.push({ seq, strategy, address, started_unix_ms: started, timed_out: false, trace: JSON.parse(json) });
    } catch (error) {
      out.push({ seq, strategy, address, started_unix_ms: started, timed_out: true, error: String(error) });
    } finally { clearTimeout(timer); }
    window.progress = out.length;
  }
  client.close();
  return { connectMs, dialsOk: dials.filter((d) => d.ok).length, dialsFailed: dials.filter((d) => !d.ok).length, reads: out };
};
window.ready = true;
