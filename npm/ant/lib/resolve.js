'use strict';

// Shared platform resolution for the @withautonomi/ant meta package.
//
// The meta package carries no binary. Each supported platform has its own package holding the
// `ant` binary from the matching GitHub release archive, declared as an optionalDependency with
// `os`/`cpu` guards, so `npm install` fetches exactly one of them. This module answers "which
// one, and where did npm put it?".

const fs = require('node:fs');
const path = require('node:path');

// `${process.platform}-${process.arch}` -> platform package name.
//
// Mirrors the release matrix in .github/workflows/ant-cli-release.yml. If a target is added
// there, add it here, to PLATFORM_TARGETS in build-packages.sh, and to the optionalDependencies
// in package.json.tmpl.
const PACKAGES = {
  'darwin-arm64': '@withautonomi/ant-darwin-arm64',
  'darwin-x64': '@withautonomi/ant-darwin-x64',
  'linux-arm64': '@withautonomi/ant-linux-arm64',
  'linux-x64': '@withautonomi/ant-linux-x64',
  'win32-x64': '@withautonomi/ant-win32-x64',
  // There is no native Windows ARM64 build. install.ps1 hands ARM64 users the x86_64 binary to
  // run under emulation (install.ps1:160-162); npm does the same, via an extra `arm64` entry in
  // the win32 package's `cpu` list.
  'win32-arm64': '@withautonomi/ant-win32-x64',
};

function platformKey() {
  return `${process.platform}-${process.arch}`;
}

function packageName(key = platformKey()) {
  return PACKAGES[key] || null;
}

function supportedPlatforms() {
  return Object.keys(PACKAGES).sort();
}

// Absolute path to the installed platform package, or null if npm did not install one.
//
// Resolution is anchored at this file rather than the cwd so it follows the dependency edge from
// the meta package, which is what npm actually laid out — global installs, local installs and
// nested node_modules all work out of the same lookup.
function packageDir(key = platformKey()) {
  const name = packageName(key);
  if (!name) return null;
  try {
    return path.dirname(require.resolve(`${name}/package.json`, { paths: [__dirname] }));
  } catch {
    return null;
  }
}

function binaryName() {
  return process.platform === 'win32' ? 'ant.exe' : 'ant';
}

// Absolute path to the `ant` executable, or null if it is not installed.
function binaryPath(key = platformKey()) {
  const dir = packageDir(key);
  if (!dir) return null;
  const file = path.join(dir, 'bin', binaryName());
  return fs.existsSync(file) ? file : null;
}

// npm normally preserves the executable bit through pack/publish/install, but registries,
// mirrors and unusual umasks have all been known to drop it, and `--ignore-scripts` rules out
// fixing it at install time. Restoring it here costs one stat on a path we are about to exec
// anyway, and turns an opaque EACCES into a working command.
function ensureExecutable(file) {
  if (process.platform === 'win32') return;
  try {
    fs.accessSync(file, fs.constants.X_OK);
    return;
  } catch {
    // Not executable — fall through and try to fix it.
  }
  try {
    fs.chmodSync(file, 0o755);
  } catch {
    // Best effort. If this fails the spawn below reports the real error.
  }
}

module.exports = {
  PACKAGES,
  binaryName,
  binaryPath,
  ensureExecutable,
  packageDir,
  packageName,
  platformKey,
  supportedPlatforms,
};
