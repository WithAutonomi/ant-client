import { defineConfig } from "@playwright/test";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const webDirectory = dirname(fileURLToPath(import.meta.url));
const antNodeDirectory = process.env.ANT_NODE_DIR
  ? resolve(process.env.ANT_NODE_DIR)
  : resolve(webDirectory, "../../ant-node-web-support");
const manifestUrl = "http://127.0.0.1:35000/api/browser-manifest.json";
const devnetLogLevel = process.env.ANT_WEBRTC_SMOKE_LOG;
if (
  devnetLogLevel &&
  !["error", "warn", "info", "debug", "trace"].includes(devnetLogLevel)
) {
  throw new Error("ANT_WEBRTC_SMOKE_LOG must be error, warn, info, debug, or trace");
}

// JSON string quoting is accepted by the shells used by Cargo's supported
// desktop platforms and keeps workspace paths containing spaces intact.
const nodeManifest = JSON.stringify(resolve(antNodeDirectory, "Cargo.toml"));
const devnetData = JSON.stringify(resolve(webDirectory, ".playwright-devnet"));
const devnetCommand = [
  "cargo run",
  `--manifest-path ${nodeManifest}`,
  "--features webrtc-direct",
  "--bin ant-devnet --",
  "--preset minimal",
  `--data-dir ${devnetData}`,
  "--base-port 33000",
  "--webrtc-direct",
  "--webrtc-direct-base-port 34000",
  "--serve-port 35000",
  "--enable-evm",
  ...(devnetLogLevel
    ? ["--enable-logging", `--log-level ${devnetLogLevel}`]
    : []),
].join(" ");

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  expect: { timeout: 30_000 },
  reporter: "line",
  use: {
    baseURL: "http://127.0.0.1:35173",
    browserName: "chromium",
    headless: true,
  },
  webServer: [
    {
      command: devnetCommand,
      url: manifestUrl,
      timeout: 300_000,
      reuseExistingServer: false,
      stdout: "pipe",
      stderr: "pipe",
    },
    {
      command: "npm run dev -- --port 35173",
      url: "http://127.0.0.1:35173",
      timeout: 180_000,
      reuseExistingServer: false,
      stdout: "pipe",
      stderr: "pipe",
    },
  ],
});
