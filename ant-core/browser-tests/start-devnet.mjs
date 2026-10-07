import { execFileSync, spawn } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
const node = resolve(process.env.ANT_NODE_DIR ?? "../../../ant-node-web-support");
const cargo = ["run", "--manifest-path", join(node, "Cargo.toml"), "--bin", "ant-devnet"];
// Without an explicit ANT_NODE_DIR the suite runs exactly the pinned node: that revision, without tracked edits,
// on its own lockfile.
if (!process.env.ANT_NODE_DIR) {
  const pinned = (await readFile("node-revision", "utf8")).trim();
  const git = (...args) => execFileSync("git", ["-C", node, ...args], { encoding: "utf8", stdio: "pipe" }).trim();
  let problem;
  try {
    const revision = git("rev-parse", "HEAD");
    if (revision !== pinned) problem = `is at ${revision}, not the pinned ${pinned}`;
    else if (git("status", "--porcelain", "--untracked-files=no") !== "") problem = `has tracked edits to the pinned ${pinned}`;
  } catch { problem = `is not a git checkout of the pinned ${pinned}`; }
  if (problem) {
    console.error(`${node} ${problem}: check out node-revision there or set ANT_NODE_DIR`);
    process.exit(1);
  }
  console.log(`Starting ant-devnet from the pinned node ${pinned}`);
  cargo.push("--locked");
}
const directory = await mkdtemp(join(tmpdir(), "ant-core-browser-test-"));
const child = spawn("cargo", [...cargo, "--",
  "--nodes", "7", "--data-dir", directory, "--base-port", "33000", "--webrtc-direct", "--webrtc-direct-base-port", "34000",
  "--serve-port", "35000", "--enable-evm"], { stdio: "inherit" });
for (const signal of ["SIGINT", "SIGTERM"]) process.on(signal, () => child.kill("SIGINT"));
child.on("exit", async code => { await rm(directory, { recursive: true, force: true }); process.exit(code ?? 1); });
