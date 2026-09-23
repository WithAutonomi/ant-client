#!/usr/bin/env bash
#
# Builds the six npm packages that make up the `ant` npm distribution, from the artifacts the
# release workflow has already built and signed.
#
# The binaries are copied verbatim out of the release archives — never rebuilt — so the published
# packages contain byte-identical copies of the GitHub release assets. Both the SHA256SUMS.txt
# checksums and the ML-DSA-65 detached signatures are verified first, and any mismatch aborts.
#
# Usage:
#   npm/build-packages.sh --version 0.3.6 --artifacts <dir> --out <dir> [options]
#
#   --version VERSION     Release version, without the `ant-cli-v` tag prefix.
#   --artifacts DIR       Directory holding ant-VERSION-TARGET.{tar.gz,zip}, their .sig files
#                         and SHA256SUMS.txt.
#   --out DIR             Directory to write the package trees into.
#   --keygen PATH         ant-keygen binary used to verify signatures. Defaults to $ANT_KEYGEN,
#                         then to `ant-keygen` on PATH.
#   --skip-signature-verification
#                         Skip ML-DSA verification. Checksums are still verified. For local
#                         dry-runs against archives you built yourself, which have no signature.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PUBLIC_KEY="${REPO_ROOT}/resources/release-signing-key.pub"
SIGNING_CONTEXT="ant-release-v1"

# rust-target | npm-suffix | os list | cpu list | archive ext | binary | human description
#
# Mirrors the build matrix in .github/workflows/ant-cli-release.yml:18-36. Adding a target means
# updating this table, PACKAGES in npm/ant/lib/resolve.js, and optionalDependencies in
# npm/ant/package.json.tmpl.
#
# win32-x64 lists arm64 as well: there is no native Windows ARM64 build, and install.ps1:160-162
# already hands ARM64 users the x86_64 binary to run under emulation. npm matches that rather
# than leaving those users with no package at all.
PLATFORM_TARGETS=(
  'x86_64-unknown-linux-musl|linux-x64|"linux"|"x64"|tar.gz|ant|Linux x86_64'
  'aarch64-unknown-linux-musl|linux-arm64|"linux"|"arm64"|tar.gz|ant|Linux ARM64'
  'x86_64-apple-darwin|darwin-x64|"darwin"|"x64"|tar.gz|ant|macOS x86_64'
  'aarch64-apple-darwin|darwin-arm64|"darwin"|"arm64"|tar.gz|ant|macOS ARM64 (Apple silicon)'
  'x86_64-pc-windows-msvc|win32-x64|"win32"|"x64", "arm64"|zip|ant.exe|Windows x86_64'
)

VERSION=""
ARTIFACTS_DIR=""
OUT_DIR=""
KEYGEN="${ANT_KEYGEN:-}"
SKIP_SIGNATURES=0

die() { echo "error: $*" >&2; exit 1; }
say() { echo "==> $*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version)   VERSION="${2:-}"; shift 2 ;;
    --artifacts) ARTIFACTS_DIR="${2:-}"; shift 2 ;;
    --out)       OUT_DIR="${2:-}"; shift 2 ;;
    --keygen)    KEYGEN="${2:-}"; shift 2 ;;
    --skip-signature-verification) SKIP_SIGNATURES=1; shift ;;
    -h|--help)   sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)           die "unknown argument: $1" ;;
  esac
done

[ -n "$VERSION" ] || die "--version is required"
[ -n "$ARTIFACTS_DIR" ] || die "--artifacts is required"
[ -n "$OUT_DIR" ] || die "--out is required"
[ -d "$ARTIFACTS_DIR" ] || die "artifacts directory does not exist: $ARTIFACTS_DIR"

ARTIFACTS_DIR="$(cd "$ARTIFACTS_DIR" && pwd)"
SUMS_FILE="${ARTIFACTS_DIR}/SHA256SUMS.txt"
[ -f "$SUMS_FILE" ] || die "SHA256SUMS.txt not found in $ARTIFACTS_DIR"

if [ "$SKIP_SIGNATURES" -eq 0 ]; then
  if [ -z "$KEYGEN" ]; then
    KEYGEN="$(command -v ant-keygen || true)"
  fi
  [ -n "$KEYGEN" ] && [ -x "$KEYGEN" ] || die \
    "ant-keygen not found. Pass --keygen PATH, set ANT_KEYGEN, or pass --skip-signature-verification."
  [ -f "$PUBLIC_KEY" ] || die "public key not found: $PUBLIC_KEY"
fi

# `sha256sum` on Linux, `shasum -a 256` on macOS. Same output format, so -c works with either.
sha256_check() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$1"
  else
    shasum -a 256 -c "$1"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

# ---------------------------------------------------------------------------
# 1. Verify every archive against SHA256SUMS.txt, and its signature.
# ---------------------------------------------------------------------------

EXPECTED_SUMS="${WORK_DIR}/expected-sums.txt"
: > "$EXPECTED_SUMS"

for entry in "${PLATFORM_TARGETS[@]}"; do
  IFS='|' read -r target _suffix _os _cpu ext _binary _desc <<< "$entry"
  archive="ant-${VERSION}-${target}.${ext}"
  [ -f "${ARTIFACTS_DIR}/${archive}" ] || die "missing release archive: ${archive}"

  # Pull this archive's line out of SHA256SUMS.txt by exact filename match, so a file listed but
  # absent (or present but unlisted) is caught rather than silently skipped.
  line="$(awk -v f="$archive" '$2 == f || $2 == "*" f { print; found = 1 } END { exit !found }' \
    "$SUMS_FILE")" || die "${archive} is not listed in SHA256SUMS.txt"
  printf '%s\n' "$line" >> "$EXPECTED_SUMS"

  if [ "$SKIP_SIGNATURES" -eq 0 ]; then
    [ -f "${ARTIFACTS_DIR}/${archive}.sig" ] || die "missing signature: ${archive}.sig"
    sig_line="$(awk -v f="${archive}.sig" '$2 == f || $2 == "*" f { print; found = 1 } END { exit !found }' \
      "$SUMS_FILE")" || die "${archive}.sig is not listed in SHA256SUMS.txt"
    printf '%s\n' "$sig_line" >> "$EXPECTED_SUMS"
  fi
done

say "Verifying checksums against SHA256SUMS.txt"
( cd "$ARTIFACTS_DIR" && sha256_check "$EXPECTED_SUMS" ) || die "checksum verification failed"

if [ "$SKIP_SIGNATURES" -eq 0 ]; then
  say "Verifying ML-DSA-65 signatures with $KEYGEN"
  for entry in "${PLATFORM_TARGETS[@]}"; do
    IFS='|' read -r target _suffix _os _cpu ext _binary _desc <<< "$entry"
    archive="${ARTIFACTS_DIR}/ant-${VERSION}-${target}.${ext}"
    "$KEYGEN" verify \
      --key "$PUBLIC_KEY" \
      --input "$archive" \
      --signature "${archive}.sig" \
      --context "$SIGNING_CONTEXT" \
      || die "signature verification failed for $(basename "$archive")"
  done
else
  say "SKIPPING signature verification (--skip-signature-verification)"
fi

# ---------------------------------------------------------------------------
# 2. Build one package per platform from the verified archives.
# ---------------------------------------------------------------------------

render() {
  # render <template> <dest> <sed-expression>...
  local template="$1" dest="$2"; shift 2
  local args=()
  local expr
  for expr in "$@"; do args+=(-e "$expr"); done
  sed "${args[@]}" "$template" > "$dest"
}

for entry in "${PLATFORM_TARGETS[@]}"; do
  IFS='|' read -r target suffix os_list cpu_list ext binary desc <<< "$entry"

  pkg_name="@withautonomi/ant-${suffix}"
  pkg_dir="${OUT_DIR}/ant-${suffix}"
  archive="${ARTIFACTS_DIR}/ant-${VERSION}-${target}.${ext}"
  extract_dir="${WORK_DIR}/${target}"
  staged="${extract_dir}/ant-${VERSION}-${target}"

  say "Building ${pkg_name}"

  mkdir -p "$extract_dir"
  case "$ext" in
    tar.gz) tar xzf "$archive" -C "$extract_dir" ;;
    zip)
      command -v unzip >/dev/null 2>&1 || die "unzip is required to unpack ${archive}"
      unzip -q "$archive" -d "$extract_dir"
      ;;
    *) die "unhandled archive extension: $ext" ;;
  esac

  # The release archives contain a single nested ant-VERSION-TARGET/ directory holding the
  # binary and bootstrap_peers.toml (ant-cli-release.yml:71-91). Fail loudly if that changes.
  [ -d "$staged" ] || die "expected directory $(basename "$staged") inside ${archive}"
  [ -f "${staged}/${binary}" ] || die "expected ${binary} inside $(basename "$archive")"
  [ -f "${staged}/bootstrap_peers.toml" ] || die "expected bootstrap_peers.toml inside $(basename "$archive")"

  rm -rf "$pkg_dir"
  mkdir -p "${pkg_dir}/bin"
  cp "${staged}/${binary}" "${pkg_dir}/bin/${binary}"
  cp "${staged}/bootstrap_peers.toml" "${pkg_dir}/bootstrap_peers.toml"
  chmod 755 "${pkg_dir}/bin/${binary}"

  render "${SCRIPT_DIR}/platform/package.json.tmpl" "${pkg_dir}/package.json" \
    "s|__PKG_NAME__|${pkg_name}|g" \
    "s|__VERSION__|${VERSION}|g" \
    "s|__NPM_OS_LIST__|${os_list}|g" \
    "s|__NPM_CPU_LIST__|${cpu_list}|g" \
    "s|__TARGET_DESC__|${desc}|g"

  render "${SCRIPT_DIR}/platform/README.md.tmpl" "${pkg_dir}/README.md" \
    "s|__PKG_NAME__|${pkg_name}|g" \
    "s|__VERSION__|${VERSION}|g" \
    "s|__TARGET_DESC__|${desc}|g" \
    "s|__RUST_TARGET__|${target}|g"

  echo "    binary sha256: $(sha256_of "${pkg_dir}/bin/${binary}")  (${binary})"
done

# ---------------------------------------------------------------------------
# 3. Build the meta package.
# ---------------------------------------------------------------------------

say "Building @withautonomi/ant"

META_DIR="${OUT_DIR}/ant"
rm -rf "$META_DIR"
mkdir -p "${META_DIR}/bin" "${META_DIR}/lib"

cp "${SCRIPT_DIR}/ant/bin/ant.js" "${META_DIR}/bin/ant.js"
cp "${SCRIPT_DIR}/ant/lib/resolve.js" "${META_DIR}/lib/resolve.js"
cp "${SCRIPT_DIR}/ant/postinstall.js" "${META_DIR}/postinstall.js"
chmod 755 "${META_DIR}/bin/ant.js"

render "${SCRIPT_DIR}/ant/package.json.tmpl" "${META_DIR}/package.json" \
  "s|__VERSION__|${VERSION}|g"
render "${SCRIPT_DIR}/ant/README.md.tmpl" "${META_DIR}/README.md" \
  "s|__VERSION__|${VERSION}|g"

# Fail here rather than at `npm publish` if a template left a placeholder behind. Only the
# rendered text files are scanned: a compiled binary will contain byte sequences matching almost
# any pattern, this one included.
placeholders="$(find "$OUT_DIR" \( -name 'package.json' -o -name 'README.md' \) -print0 \
  | xargs -0 grep -n '__[A-Z_]\{1,\}__' 2>/dev/null || true)"
if [ -n "$placeholders" ]; then
  printf '%s\n' "$placeholders" >&2
  die "unsubstituted template placeholders remain in $OUT_DIR"
fi

say "Done. Packages written to ${OUT_DIR}:"
for entry in "${PLATFORM_TARGETS[@]}"; do
  IFS='|' read -r _target suffix _os _cpu _ext _binary _desc <<< "$entry"
  echo "    ant-${suffix}"
done
echo "    ant  (meta package — publish last)"
