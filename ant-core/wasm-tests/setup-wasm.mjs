import { readFile } from "node:fs/promises";
import { Worker } from "node:worker_threads";
import initAntCore from "./pkg/ant_core.js";

// A test that stops yielding blocks every timer in this process, node:test's
// own timeout included, so a regression like V2-1305 would hang the run for as
// long as the loop spins. A worker has its own event loop: when the main
// thread misses its heartbeat for this long, the worker reports it and kills
// the process, so the test file fails promptly with a reason.
const EVENT_LOOP_STALL_LIMIT_MS = 60_000;
const HEARTBEAT_INTERVAL_MS = 250;
const WATCHDOG_POLL_MS = 1_000;

const heartbeat = new Int32Array(new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT));
setInterval(() => Atomics.add(heartbeat, 0, 1), HEARTBEAT_INTERVAL_MS).unref();
new Worker(`
  const { workerData } = require("node:worker_threads");
  const { writeSync } = require("node:fs");
  const { heartbeat, limitMs, pollMs, script } = workerData;
  let last = Atomics.load(heartbeat, 0);
  let stalledSince = Date.now();
  setInterval(() => {
    const beat = Atomics.load(heartbeat, 0);
    if (beat !== last) {
      last = beat;
      stalledSince = Date.now();
    } else if (Date.now() - stalledSince >= limitMs) {
      // Written synchronously: this thread's console is relayed through the
      // blocked main thread.
      writeSync(2, "\\n" + script + ": the event loop was blocked for " + limitMs +
        " ms; a test stopped yielding (a V2-1305-style spin). Killing the process.\\n");
      process.kill(process.pid, "SIGKILL");
    }
  }, pollMs);
`, {
  eval: true,
  workerData: {
    heartbeat,
    limitMs: EVENT_LOOP_STALL_LIMIT_MS,
    pollMs: WATCHDOG_POLL_MS,
    script: process.argv[1] ?? "wasm test",
  },
}).unref();

const wasm = await readFile(new URL("./pkg/ant_core_bg.wasm", import.meta.url));
await initAntCore({ module_or_path: wasm });
