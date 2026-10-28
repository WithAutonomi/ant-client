import { test, expect } from "@playwright/test";
// Two Chromium pages, in separate contexts, read one file. Nodes that host
// swarm rooms introduce them and answer their STUN, and the second reads the
// file from the first; older nodes are never sent a swarm message, and the
// second reads it from them as before.
test("real Chromium pages reading one file share its records, or read it from older nodes", async ({ browser, request }) => {
  const manifest = await (await request.get("http://127.0.0.1:35000/api/browser-manifest.json")).json();
  const info = await (await request.get("http://127.0.0.1:35000/api/info")).json();
  const options = { endpoint: manifest.endpoints[0], payment: manifest.payment, rpcUrl: info.evm.rpc_url };
  const errors = [];
  const contexts = [];
  const open = async () => {
    const context = await browser.newContext();
    contexts.push(context);
    const page = await context.newPage();
    page.on("pageerror", error => errors.push(error.message));
    await page.goto("/");
    await page.waitForFunction(() => typeof globalThis.runSwarmRead === "function");
    return page;
  };
  const first = await open();
  const file = await first.evaluate(o => globalThis.runSwarmUpload(o), { ...options, size: 6 * 1024 * 1024 });
  const seeded = await first.evaluate(o => globalThis.runSwarmRead(o), { ...options, address: file.address, meet: false });
  expect(seeded.sha).toBe(file.sha);
  expect(seeded.stats.records_from_peers).toBe(0);

  const swarm = seeded.capabilities.includes("swarm_rendezvous");
  // Against nodes built from the client's own ant-node, the rooms must be
  // there: only the sharing branch below proves the swarm works.
  if (process.env.REQUIRE_SWARM) expect(swarm, "the nodes host swarm rooms").toBe(true);
  const second = await open();
  const read = await second.evaluate(o => globalThis.runSwarmRead(o), { ...options, address: file.address, meet: swarm });
  expect(read.sha).toBe(file.sha);
  expect(read.stats.records_rejected).toBe(0);
  const served = await first.evaluate(() => globalThis.swarmStatsNow());
  if (swarm) {
    // Data records came from the first page (as many as one link takes at
    // once: one asked, one waiting; further reads at once go to the nodes,
    // which a busy machine may also let win a race), and the DataMap, which
    // names the room, from the nodes.
    expect(read.stats.records_from_peers, JSON.stringify({ read: read.stats, served }))
      .toBeGreaterThanOrEqual(1);
    // A record may be sent whole on a link just closed as the duplicate of
    // another between the two pages, and so served but not read.
    expect(served.records_served).toBeGreaterThanOrEqual(read.stats.records_from_peers);
    expect(served.bytes_served).toBeGreaterThanOrEqual(read.stats.bytes_from_peers);
    // Whole records: this file's are 2 MiB each.
    expect(read.stats.bytes_from_peers).toBeGreaterThan(read.stats.records_from_peers * 1024 * 1024);
  } else {
    expect(read.stats.records_from_peers).toBe(0);
    expect(read.stats.links).toBe(0);
    expect(served.records_served).toBe(0);
  }
  expect(errors).toEqual([]);
  for (const context of contexts) await context.close();
});
