#!/usr/bin/env bash
# Quick-start installer for the Autonomi `ant` CLI.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/WithAutonomi/ant-client/main/install.sh | bash
#
# Environment variables:
#   ANT_CHANNEL        — release channel to install from: "stable" (default) or "beta".
#   ANT_VERSION        — install a specific version (e.g. "0.1.1"). Overrides ANT_CHANNEL.
#   INSTALL_DIR        — override install directory (default: ~/.local/bin).
#   ANT_NO_MODIFY_PATH — set to 1 to leave your shell startup files alone (same as --no-modify-path).
#
# Options:
#   --no-modify-path   — do not add INSTALL_DIR to PATH; print the line to add instead.
#
# When INSTALL_DIR is not already on PATH, the installer adds it to your shell startup files
# (~/.profile, your existing bash files, ~/.zshenv, fish's conf.d) so that `ant` works in every new
# terminal, and prints each file it changed. install.ps1 does the same with the Windows user PATH.
#
# Examples:
#   curl -fsSL <this-script> | bash                           # newest stable
#   curl -fsSL <this-script> | ANT_CHANNEL=beta bash          # newest beta-eligible
#   curl -fsSL <this-script> | ANT_VERSION=0.3.3 bash         # a specific version
#   curl -fsSL <this-script> | bash -s -- --no-modify-path    # leave PATH to me

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

# User-writable on both platforms, so the install never needs sudo. macOS used to default to
# /usr/local/bin, which is root-owned on a machine without Homebrew.
default_install_dir() {
  echo "$HOME/.local/bin"
}

# $1 with a leading $HOME spelled as a literal `$HOME`, for lines written into startup files.
home_relative() {
  case "$1" in
    "$HOME"/*) printf '%s\n' "\$HOME${1#"$HOME"}" ;;
    *)         printf '%s\n' "$1" ;;
  esac
}

# Shell startup files that should put INSTALL_DIR on PATH, one per line. Mirrors rustup:
#   ~/.profile          — every POSIX login shell; always written.
#   ~/.bash_profile,
#   ~/.bash_login       — bash login shells skip ~/.profile when either exists; written if present.
#   ~/.bashrc           — interactive non-login bash (a new Linux terminal); written if present, or
#                         created when bash is the login shell.
#   ~/.zshenv           — read by every zsh, interactive or not; written whenever zsh is installed,
#                         which includes every Mac.
# fish is handled separately, as its syntax differs.
posix_startup_files() {
  local shell_name="${SHELL##*/}" rc

  printf '%s\n' "$HOME/.profile"
  for rc in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.bashrc"; do
    if [ -f "$rc" ]; then
      printf '%s\n' "$rc"
    elif [ "$rc" = "$HOME/.bashrc" ] && [ "$shell_name" = "bash" ]; then
      printf '%s\n' "$rc"
    fi
  done
  if [ "$shell_name" = "zsh" ] || command -v zsh > /dev/null 2>&1; then
    printf '%s\n' "${ZDOTDIR:-$HOME}/.zshenv"
  fi
}

# Append line $2 to startup file $1, under a comment saying where it came from.
# Returns 0 when the file was changed, 1 when the line was already there, 2 when the write failed.
append_startup_line() {
  local rc="$1" line="$2" gap=""

  if [ -f "$rc" ] && grep -qxF "$line" "$rc"; then
    return 1
  fi
  if [ -s "$rc" ]; then
    gap=$'\n'
  fi
  {
    mkdir -p "$(dirname "$rc")" \
      && printf '%s# Added by the ant installer (https://github.com/%s)\n%s\n' "$gap" "$REPO" "$line" >> "$rc"
  } 2> /dev/null || return 2
}

# Put INSTALL_DIR on PATH for future shells, and tell the user what changed.
add_install_dir_to_path() {
  local dir posix_line fish_line rc i status
  local entries=() changed=() present=() failed=()

  dir="$(home_relative "$INSTALL_DIR")"
  # Guarded, so a shell that reads several of these files (Ubuntu's ~/.profile sources ~/.bashrc)
  # still ends up with one entry.
  # shellcheck disable=SC2016 # $PATH is meant to reach the startup file unexpanded.
  posix_line="$(printf 'case ":${PATH}:" in *":%s:"*) ;; *) export PATH="%s:$PATH" ;; esac' "$dir" "$dir")"
  # shellcheck disable=SC2016
  fish_line="$(printf 'contains "%s" $PATH; or set -gx PATH "%s" $PATH' "$dir" "$dir")"

  # Pairs of (startup file, line to add to it).
  while IFS= read -r rc; do
    entries+=("$rc" "$posix_line")
  done < <(posix_startup_files)
  if command -v fish > /dev/null 2>&1; then
    entries+=("${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/ant.fish" "$fish_line")
  fi

  for ((i = 0; i < ${#entries[@]}; i += 2)); do
    rc="${entries[i]}"
    status=0
    append_startup_line "$rc" "${entries[i + 1]}" || status=$?
    case "$status" in
      0) changed+=("$rc") ;;
      1) present+=("$rc") ;;
      *) failed+=("$rc") ;;
    esac
  done

  say ""
  if [ "${#changed[@]}" -gt 0 ]; then
    say "Added ${INSTALL_DIR} to your PATH in:"
    for rc in "${changed[@]}"; do say "  $rc"; done
  elif [ "${#present[@]}" -gt 0 ]; then
    say "Your shell startup files already add ${INSTALL_DIR} to your PATH."
  fi
  if [ "${#failed[@]}" -gt 0 ]; then
    say "Could not write to:"
    for rc in "${failed[@]}"; do say "  $rc"; done
  fi
  if [ "${#changed[@]}" -eq 0 ] && [ "${#present[@]}" -eq 0 ]; then
    say "Add it to your PATH with:"
  else
    say "Open a new terminal to use ant, or run this to use it in the current one:"
  fi
  say "  export PATH=\"${INSTALL_DIR}:\$PATH\""
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

MODIFY_PATH=1
case "${ANT_NO_MODIFY_PATH:-}" in
  ""|0) ;;
  *) MODIFY_PATH=0 ;;
esac
for arg in "$@"; do
  case "$arg" in
    --no-modify-path) MODIFY_PATH=0 ;;
    *) err "unknown option '$arg' (the only option is --no-modify-path)" ;;
  esac
done

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
# Absolute, so the PATH entry written below does not depend on where the installer was run from.
INSTALL_DIR="$(cd "$INSTALL_DIR" && pwd)"
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
    if [ "$MODIFY_PATH" = 1 ]; then
      add_install_dir_to_path
    else
      say ""
      say "WARNING: ${INSTALL_DIR} is not in your PATH."
      say "Add it with:"
      say "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi
    ;;
esac

# Another `ant` can shadow this one: an older install (macOS used to default to /usr/local/bin), or
# an unrelated program such as Apache Ant. macOS login shells put /usr/local/bin and Homebrew's bin
# ahead of ~/.local/bin, so either wins there.
OTHER_ANT="$(command -v "$BINARY_NAME" 2> /dev/null || true)"
if [ -n "$OTHER_ANT" ] && [ "$OTHER_ANT" != "${INSTALL_DIR}/${BINARY_NAME}" ]; then
  say ""
  say "NOTE: ${OTHER_ANT} is also on your PATH and may run instead of this ant."
  say "If it is an older install of this ant, remove it. If it is another program (Apache Ant is"
  say "also called ant), put ${INSTALL_DIR} first on your PATH or run ${INSTALL_DIR}/${BINARY_NAME}."
fi

say ""
say "Done! Run 'ant --help' to get started."
