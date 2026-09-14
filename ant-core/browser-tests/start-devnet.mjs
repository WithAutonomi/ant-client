import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
const directory = await mkdtemp(join(tmpdir(), "ant-core-browser-test-"));
const node = resolve(process.env.ANT_NODE_DIR ?? "../../../ant-node-web-support");
const child = spawn("cargo", ["run", "--manifest-path", join(node, "Cargo.toml"), "--bin", "ant-devnet", "--",
  "--nodes", "7", "--data-dir", directory, "--base-port", "33000", "--webrtc-direct", "--webrtc-direct-base-port", "34000",
  "--serve-port", "35000", "--enable-evm"], { stdio: "inherit" });
for (const signal of ["SIGINT", "SIGTERM"]) process.on(signal, () => child.kill("SIGINT"));
child.on("exit", async code => { await rm(directory, { recursive: true, force: true }); process.exit(code ?? 1); });
