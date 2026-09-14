'use strict';

// Installs bootstrap_peers.toml into the platform config directory.
//
// Deliberately mirrors install.sh:216-224 and install.ps1:189-198: same destination, and the
// file is written only when absent so a user's edited peer list is never overwritten. The
// source is the copy inside the platform package, which came verbatim from the release archive,
// so an npm install and an install.sh install leave the same bytes in the same place.
//
// ant-core only ever reads this file (ant-core/src/config.rs:64-81); without it,
// resolve_bootstrap_peers() fails with NoBootstrapPeers.
//
// KNOWN GAP: npm skips postinstall under `--ignore-scripts` (and with ignore-scripts=true in
// .npmrc). `ant` still installs and runs there, but network commands fail until the user
// supplies peers with `-b` or writes this file themselves. See npm/README.md.
//
// This script must never fail the install. npm treats a non-zero postinstall as a failed
// install, and a missing peers file is recoverable while a failed install is not.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const { packageDir } = require('./lib/resolve.js');

const CONFIG_FILE = 'bootstrap_peers.toml';

function say(message) {
  process.stdout.write(`@withautonomi/ant: ${message}\n`);
}

function warn(message) {
  process.stderr.write(`@withautonomi/ant: ${message}\n`);
}

// Mirrors config_dir() in install.sh:59-67, Get-ConfigDir in install.ps1:149-151, and
// ant_core::config::config_dir() in ant-core/src/config.rs:32-45. All four must agree.
function configDir() {
  if (process.platform === 'win32') {
    const appData = process.env.APPDATA;
    if (!appData) throw new Error('APPDATA is not set');
    return path.join(appData, 'ant');
  }
  if (process.platform === 'darwin') {
    return path.join(os.homedir(), 'Library', 'Application Support', 'ant');
  }
  const xdg = process.env.XDG_CONFIG_HOME;
  return path.join(xdg && xdg.length > 0 ? xdg : path.join(os.homedir(), '.config'), 'ant');
}

function main() {
  const dir = packageDir();
  if (!dir) {
    // No platform package: either an unsupported platform or --no-optional. bin/ant.js explains
    // it properly when the command is actually run; do not fail the install over it here.
    return;
  }

  const source = path.join(dir, CONFIG_FILE);
  if (!fs.existsSync(source)) {
    warn(`${CONFIG_FILE} is missing from the platform package; skipping bootstrap config.`);
    return;
  }

  const destDir = configDir();
  const dest = path.join(destDir, CONFIG_FILE);

  if (fs.existsSync(dest)) {
    say(`bootstrap config already exists at ${dest} - skipping`);
    return;
  }

  fs.mkdirSync(destDir, { recursive: true });
  fs.copyFileSync(source, dest);
  say(`installed bootstrap config to ${dest}`);
}

try {
  main();
} catch (err) {
  warn(
    `could not install ${CONFIG_FILE}: ${err.message}\n` +
      `  ant is installed and usable; supply peers with 'ant -b <multiaddr> ...' or create the\n` +
      `  file yourself. See https://github.com/WithAutonomi/ant-client#configuration`
  );
}
