#!/usr/bin/env node
'use strict';

// Launcher for the `ant` binary installed by the matching platform package.
//
// A postinstall script could copy the binary here and let npm exec it directly, saving this
// process. It is deliberately not done that way: `npm install --ignore-scripts` is common in
// agent sandboxes and locked-down CI, and this package must work there. A JS launcher always
// runs, at the cost of Node's startup — negligible for a network-bound CLI.

const { spawnSync } = require('node:child_process');
const {
  binaryPath,
  packageName,
  platformKey,
  supportedPlatforms,
  ensureExecutable,
} = require('../lib/resolve.js');

function fail(message) {
  process.stderr.write(`ant: ${message}\n`);
  process.exit(1);
}

const key = platformKey();
const binary = binaryPath(key);

if (!binary) {
  const pkg = packageName(key);
  if (!pkg) {
    fail(
      `no prebuilt binary for ${key}.\n` +
        `  Supported platforms: ${supportedPlatforms().join(', ')}\n` +
        `  Build from source instead: https://github.com/WithAutonomi/ant-client`
    );
  }
  fail(
    `the platform package ${pkg} is not installed.\n` +
      `  Reinstall with:  npm install -g @withautonomi/ant\n` +
      `  Note that --no-optional and --omit=optional prevent the binary from being fetched.`
  );
}

ensureExecutable(binary);

const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });

if (result.error) {
  fail(`failed to run ${binary}: ${result.error.message}`);
}

// Re-raise rather than translating to an exit code, so callers see the CLI's own termination
// reason (a Ctrl-C on a long upload must look like a Ctrl-C to the shell).
if (result.signal) {
  process.kill(process.pid, result.signal);
  // Reached only if the signal is ignored or blocked.
  process.exit(1);
}

process.exit(result.status === null ? 1 : result.status);
