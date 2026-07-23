# Changelog

All notable changes to the `ant` binary will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- `ant node start`/`ant node stop` with `--service-name` now resolve the node ID through the daemon API instead of reading `node_registry.json` directly, eliminating a race against concurrent registry mutations by the daemon.
- `ant node add --json` no longer interleaves binary-download progress with the JSON result; progress output now goes to stderr (was stdout) and is suppressed entirely in JSON mode.

### Changed
- Default network binding changed from IPv4-only to IPv6 dual-stack. Hosts without a working IPv6 stack should pass `--ipv4-only` to avoid advertising unreachable v6 addresses to the DHT (which causes slow connects and junk address records).
- `ant file upload` now writes datamaps as `<filename>.<extension>.datamap` instead of stripping the extension. Uploading `photo.jpg` produces `photo.jpg.datamap` (was `photo.datamap`). Existing datamaps remain readable.
- `ant file upload` no longer silently overwrites an existing datamap. Repeated uploads of the same source path produce `name-2.datamap`, `name-3.datamap`, … capped at 100 attempts. Pass `--overwrite` to restore the previous behaviour.

### Added
- `ant file upload --overwrite`: replace any existing `<filename>.datamap` rather than writing a suffixed sibling.
- `ant file download --datamap` no longer requires `-o/--output` — defaults to the original filename derived from the datamap basename (`photo.jpg.datamap` → `photo.jpg`, written to the current directory). Pass `-o` to override.
- `ant file download --datamap` now reads both msgpack (canonical) and legacy JSON datamaps, so datamaps produced by older versions of the GUI download cleanly via the CLI.

### Internal
- CLI-audit thinning (V2-189): `PortRange` parsing (`FromStr`), env `KEY=VALUE` parsing (`AddNodeOpts::parse_env_vars`), and bootstrap-peer resolution (`config::resolve_bootstrap_peers`) moved from ant-cli into ant-core; `node add`/`node reset` daemon calls now go through `ant_core::node::daemon::client` (new `add_node`/`reset`/`resolve_node_id_by_name` functions) instead of hand-rolled HTTP; the two CLI `ProgressReporter` impls collapsed into one.
- New `ant_core::datamap_file` module owns the on-disk datamap format (msgpack canonical, JSON legacy auto-detect on read) and naming convention. `ant-cli` and consumers like `ant-gui` route through this single helper instead of reimplementing serialization.

## [0.1.1] - 2026-03-28

### Added
- Node management: `ant node add`, `ant node start`, `ant node stop`, `ant node status`, `ant node reset`
- Daemon management: `ant node daemon start`, `ant node daemon stop`, `ant node daemon status`
- Data operations: `ant file upload`, `ant file download`, `ant chunk put`, `ant chunk get`
- Wallet management: `ant wallet address`, `ant wallet balance`
- Automatic bootstrap peer loading from `bootstrap_peers.toml` config file
- `--json` global flag for structured output
- Cross-platform support (Linux, macOS, Windows)
