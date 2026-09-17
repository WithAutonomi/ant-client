import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: ".", testMatch: "*.spec.js", workers: 1, timeout: 300_000, reporter: "line",
  use: { browserName: "chromium", headless: true, baseURL: "http://127.0.0.1:35173",
    launchOptions: { args: ["--force-fieldtrials=WebRTC-NoSdpMangleUfrag/Enabled/"] } },
  webServer: [
    { command: "node start-devnet.mjs", url: "http://127.0.0.1:35000/api/info", timeout: 900_000, reuseExistingServer: false, stdout: "pipe", stderr: "pipe" },
    { command: "npm run serve", url: "http://127.0.0.1:35173", timeout: 60_000, reuseExistingServer: false },
  ],
});
