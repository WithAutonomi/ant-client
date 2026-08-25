#!/usr/bin/env bash
# Quick-start installer for the Autonomi `ant` CLI.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/WithAutonomi/ant-client/main/install.sh | bash
#
# Environment variables:
#   ANT_CHANNEL   — release channel to install from: "stable" (default) or "beta".
#   ANT_VERSION   — install a specific version (e.g. "0.1.1"). Overrides ANT_CHANNEL.
#   INSTALL_DIR   — override install directory (default: ~/.local/bin on Linux, /usr/local/bin on macOS).
#
# Examples:
#   curl -fsSL <this-script> | bash                     # newest stable
#   curl -fsSL <this-script> | ANT_CHANNEL=beta bash    # newest beta-eligible
#   curl -fsSL <this-script> | ANT_VERSION=0.3.3 bash   # a specific version

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
    MINGW*|MSYS*|CYGWIN*)
      err "this installer is for Linux/macOS. On Windows, run in PowerShell:
  irm https://raw.githubusercontent.com/WithAutonomi/ant-client/main/install.ps1 | iex"
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

# Whether a version is eligible for a release channel.
#
# Deliberately mirrors `version_matches_channel` in ant-core/src/channel.rs, which in turn mirrors
# ant-node's own copy. If these drift, the installer hands someone a build that `ant update` would
# then refuse to move off, or vice versa.
#
#   stable — final releases only, i.e. no pre-release component at all.
#   beta   — final releases, plus pre-releases whose first identifier is exactly `beta`.
#
# Every other pre-release suffix is rejected on both channels. `-rc.*` in particular is NOT a beta
# candidate: release candidates are published before the release gates have reported, and semver
# ranks `-rc` above `-beta`, so accepting them would pull beta users onto un-gated code. Both
# 0.3.4-beta.1 and 0.3.4-rc.1 routinely exist at once, which is exactly when this matters.
version_matches_channel() {
  local version="$1" channel="$2" pre=""

  [ "$version" != "${version%%-*}" ] && pre="${version#*-}"

  # A final release is eligible on every channel.
  [ -z "$pre" ] && return 0
  [ "$channel" = "beta" ] || return 1
  # `%%.*` rather than a prefix match, so `betamax.1` is not mistaken for a beta.
  [ "${pre%%.*}" = "beta" ]
}

# True when $1 is a greater version than $2, by semver precedence.
#
# `sort -V` is not used: it does not implement semver's rule that a pre-release ranks *below* the
# release it was cut from, and BSD and GNU builds disagree, which matters because this script runs
# on macOS as well as Linux.
version_gt() {
  local a="$1" b="$2"
  local a_core="${a%%-*}" b_core="${b%%-*}"
  local a_pre="" b_pre="" a_field b_field i
  local a_parts b_parts

  [ "$a" != "$a_core" ] && a_pre="${a#*-}"
  [ "$b" != "$b_core" ] && b_pre="${b#*-}"

  IFS=. read -r -a a_parts <<< "$a_core"
  IFS=. read -r -a b_parts <<< "$b_core"
  for i in 0 1 2; do
    a_field="${a_parts[$i]:-0}"
    b_field="${b_parts[$i]:-0}"
    [ "$a_field" -gt "$b_field" ] && return 0
    [ "$a_field" -lt "$b_field" ] && return 1
  done

  # Same core version: a final release outranks any pre-release of it.
  if [ -z "$a_pre" ] && [ -n "$b_pre" ]; then return 0; fi
  if [ -n "$a_pre" ] && [ -z "$b_pre" ]; then return 1; fi
  if [ -z "$a_pre" ] && [ -z "$b_pre" ]; then return 1; fi

  # Both are pre-releases, and only `beta.N` reaches this far, so the trailing number decides.
  local a_n="${a_pre##*.}" b_n="${b_pre##*.}"
  case "$a_n" in ''|*[!0-9]*) a_n=0 ;; esac
  case "$b_n" in ''|*[!0-9]*) b_n=0 ;; esac
  [ "$a_n" -gt "$b_n" ]
}

# Newest stable release, via the endpoint that already excludes pre-releases.
latest_stable_version() {
  curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' \
    | sed -E 's/.*"ant-cli-v([^"]+)".*/\1/'
}

# Highest release eligible for channel $1, scanning the release list.
#
# `/releases/latest` cannot serve the beta channel: GitHub excludes pre-releases from it entirely,
# so it would always return the newest stable build. The list endpoint includes them, and the
# highest *eligible* entry wins — which is neither the newest by date nor the highest by raw semver.
latest_channel_version() {
  local channel="$1" tags version best=""

  tags="$(
    curl -fsSL "https://api.github.com/repos/${REPO}/releases?per_page=100" \
      | grep -oE '"tag_name" *: *"ant-cli-v[^"]+"' \
      | sed -E 's/.*"ant-cli-v([^"]+)"/\1/'
  )"

  [ -n "$tags" ] || err "could not read the release list for ${REPO}"

  for version in $tags; do
    version_matches_channel "$version" "$channel" || continue
    if [ -z "$best" ] || version_gt "$version" "$best"; then
      best="$version"
    fi
  done

  [ -n "$best" ] || err "no ${channel}-eligible ant-cli release found"
  printf '%s\n' "$best"
}

resolve_version() {
  local channel="$1"
  case "$channel" in
    stable) latest_stable_version ;;
    beta)   latest_channel_version beta ;;
    *)      err "unknown channel '${channel}' (expected 'stable' or 'beta')" ;;
  esac
}

# --- main -------------------------------------------------------------------

need curl
need tar

TARGET="$(detect_target)"
CHANNEL="${ANT_CHANNEL:-stable}"
VERSION="${ANT_VERSION:-$(resolve_version "$CHANNEL")}"
INSTALL_DIR="${INSTALL_DIR:-$(default_install_dir)}"

if [ -n "${ANT_VERSION:-}" ]; then
  say "Installing ant ${VERSION} for ${TARGET}..."
else
  say "Installing ant ${VERSION} for ${TARGET} (${CHANNEL} channel)..."
fi

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
