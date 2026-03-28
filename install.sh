#!/usr/bin/env bash
# Quick-start installer for the Autonomi `ant` CLI.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/WithAutonomi/ant-client/main/install.sh | bash
#
# Environment variables:
#   ANT_VERSION   — install a specific version (e.g. "0.1.1"). Defaults to latest.
#   INSTALL_DIR   — override install directory (default: ~/.local/bin on Linux, /usr/local/bin on macOS).

set -euo pipefail

REPO="WithAutonomi/ant-client"
BINARY_NAME="ant"

# --- helpers ----------------------------------------------------------------

say() { printf '%s\n' "$*"; }
err() { say "Error: $*" >&2; exit 1; }

need() {
  command -v "$1" > /dev/null 2>&1 || err "need '$1' (command not found)"
}

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux)
      case "$arch" in
        x86_64)  echo "x86_64-unknown-linux-musl" ;;
        aarch64) echo "aarch64-unknown-linux-musl" ;;
        *)       err "unsupported Linux architecture: $arch" ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        x86_64)  echo "x86_64-apple-darwin" ;;
        arm64)   echo "aarch64-apple-darwin" ;;
        *)       err "unsupported macOS architecture: $arch" ;;
      esac
      ;;
    *) err "unsupported OS: $os" ;;
  esac
}

config_dir() {
  local os
  os="$(uname -s)"
  case "$os" in
    Linux)  echo "${XDG_CONFIG_HOME:-$HOME/.config}/ant" ;;
    Darwin) echo "$HOME/Library/Application Support/ant" ;;
    *)      err "unsupported OS: $os" ;;
  esac
}

default_install_dir() {
  local os
  os="$(uname -s)"
  case "$os" in
    Linux)  echo "$HOME/.local/bin" ;;
    Darwin) echo "/usr/local/bin" ;;
    *)      err "unsupported OS: $os" ;;
  esac
}

latest_version() {
  curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' \
    | sed -E 's/.*"ant-cli-v([^"]+)".*/\1/'
}

# --- main -------------------------------------------------------------------

need curl
need tar

TARGET="$(detect_target)"
VERSION="${ANT_VERSION:-$(latest_version)}"
INSTALL_DIR="${INSTALL_DIR:-$(default_install_dir)}"

say "Installing ant ${VERSION} for ${TARGET}..."

ARCHIVE="${BINARY_NAME}-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/ant-cli-v${VERSION}/${ARCHIVE}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

say "Downloading ${URL}..."
curl -fSL -o "${TMPDIR}/${ARCHIVE}" "$URL"

say "Extracting..."
tar xzf "${TMPDIR}/${ARCHIVE}" -C "$TMPDIR"

# Install binary
mkdir -p "$INSTALL_DIR"
cp "${TMPDIR}/${BINARY_NAME}-${VERSION}-${TARGET}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
chmod +x "${INSTALL_DIR}/${BINARY_NAME}"
say "Installed ${BINARY_NAME} to ${INSTALL_DIR}/${BINARY_NAME}"

# Install bootstrap config
CONF_DIR="$(config_dir)"
mkdir -p "$CONF_DIR"
if [ ! -f "${CONF_DIR}/bootstrap_peers.toml" ]; then
  cp "${TMPDIR}/${BINARY_NAME}-${VERSION}-${TARGET}/bootstrap_peers.toml" "${CONF_DIR}/bootstrap_peers.toml"
  say "Installed bootstrap config to ${CONF_DIR}/bootstrap_peers.toml"
else
  say "Bootstrap config already exists at ${CONF_DIR}/bootstrap_peers.toml — skipping"
fi

# Check PATH
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    say ""
    say "WARNING: ${INSTALL_DIR} is not in your PATH."
    say "Add it with:"
    say "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac

say ""
say "Done! Run 'ant --help' to get started."
