# Browser download startup investigation — 2026-09-25

Test file: `134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889`.
The supplied seven bootstrap addresses are in [seeds.json](seeds.json).

## What took the time

The supplied File Vault log separates into these phases:

| Phase | Time |
| --- | ---: |
| Fetch and verify the 320-byte public DataMap | 27.266 s |
| Resolve its three nested metadata records, through `0/147` | 105.046 s |
| First content chunk after metadata resolution | 6.049 s |
| All 147 content chunks, measured from `0/147` | 457.271 s |
| Reconstruction and verification after `147/147` | 38.263 s |

`Downloaded chunk 0/147` means the root DataMap is resolved; it is not a
downloaded content chunk. The three wrapper records must be retrieved before
the client knows the 147 content addresses. Their payloads are only about 3.3 KB
each. This explains why a metadata lookup stall delays all visible content
progress; it is not evidence of two minutes spent transferring those bytes.

Instrumented live attempts revealed several avoidable network scheduling stalls:

1. **Repeated failed WebRTC setup.** The control and data lanes share an
   association setup lock. Both checked the failure cache before waiting for the
   lock, so the second lane could dial the same failed endpoint again. In
   `baseline-single-1`, setup to `82.68.107.100` started near 7.276 s, the data
   lane queued near 7.308 s, and the second dial started near 17.283 s. The read
   moved on near 27.290 s. One unreachable peer consumed two setup allowances.
2. **A slow speculative GET blocked newer holders.** Before discovery finished,
   the engine allowed only one early GET. A cold address could occupy it for a
   full setup timeout, even as a reachable holder appeared. Publishing cold
   candidates also let them occupy both slots after adding a second early GET.
3. **Speculation stopped too soon.** In the intermediate `dial-hedge-single-1`
   trace, seven early attempts for wrapper `69443b7f…` were used by 33.72 s.
   Holder `696e6bae…` was connected at 36.69 s and answered lookup at 36.98 s.
   The client waited until 54.62 s to GET it, then received the record in about
   0.12 s. The seven-attempt allowance had been exhausted while discovery was
   still running.

These are directly observed mechanisms, not a reconstruction of every second
of the historical 105-second gap. That exact slow metadata attempt was not
reproduced. Public peer availability and network latency vary between attempts.

File Vault also reports “Connected” after a separate authenticated probe that
the SDK closes. Its data client starts cold on the first download. That lifecycle
is unchanged here. The initial 5-second connect log does not mean the data
client's routing table and connections have been warmed for the requested file.

## Changes

- Recheck failed-endpoint suppression under the shared WebRTC setup lock.
  A queued lane observes the previous lane's failed dial. Cancellation alone
  does not record a failure, and a live association remains reusable.
- Seed speculative reads from connected peers on both native and WASM.
  Browser lookup and bounded preconnection tasks publish additional hints once
  authenticated. Cold peers remain eligible for ordinary discovery and fallback.
- Allow a second speculative GET after one second on **both** targets. Preserve
  the existing maximum of two simultaneous logical GETs per record, including
  the handoff when discovery completes. Physical browser replies, including
  abandoned requests, remain subject to the existing response-memory budget.
- Use the existing twenty-peer fallback allowance during progressive discovery,
  instead of stopping at seven early attempts. This is also shared policy.

Peer deduplication, the final close-group size, two-round retry policy, content
verification and authoritative absence rules are retained. Early misses cannot
establish absence. The tradeoff is more early attempts and an additional
overlapping read during slow discovery, within the already permitted peak
logical read concurrency. No gateway, metadata format, storage protocol or
application-specific download algorithm was introduced.

The native adapter also boxes its DHT lookup futures as `BoxFuture` to keep the
expanded shared async path within Rust's trait-resolution limits while retaining
`Send`. This does not change discovery behavior.

See the amended Proposed [ADR-0004](../../adr/ADR-0004-direct-browser-read-client.md).

## Measurements

Times below are seconds from a cold browser download call; WASM initialization
and the earlier discarded SDK connection probe are excluded. All use default
adaptive concurrency, a fresh Chromium context, the same file and the **first
supplied seed only**, matching File Vault's current single-address API.

| Variant / sample | Public map | Root metadata (`0/147`) | First content chunk | Third content chunk |
| --- | ---: | ---: | ---: | ---: |
| Baseline 1 | 27.744 | 51.041 | 54.658 | 73.979 |
| Baseline 2 | 21.912 | 42.284 | 48.342 | 64.771 |
| Baseline 3 | 27.407 | 61.884 | 72.265 | 84.498 |
| Baseline 4 | 27.478 | 30.151 | 38.800 | 67.251 |
| Changed 1 | 12.320 | 16.053 | 20.229 | 27.192 |
| Changed 2 | 21.911 | 35.633 | 43.310 | 49.521 |
| Changed 3 | 26.686 | 42.819 | 49.467 | 52.989 |
| Baseline median | 27.443 | 46.662 | 51.500 | 70.615 |
| Changed median | 21.911 | 35.633 | 43.310 | 49.521 |

The first-chunk median improved about 16%; the third-chunk median improved about
30%. Baseline runs were interspersed with intermediate candidates. The final
three changed runs followed the last scheduling correction; this is a small live
sample, not a randomized benchmark or a latency guarantee. Every sample in the
table is retained in `traces/`, with machine-readable milestones in
[results.json](results.json). Earlier exploratory traces, including failed
bootstrap attempts, are retained separately and excluded from this table.

Additional checks:

- **All seven seeds, changed core, one sample:** public map 11.426 s, metadata
  15.059 s, first content chunk 17.387 s, third 18.759 s. This changes the bootstrap
  configuration, so it is not part of the single-seed comparison. File Vault's
  connection API was not changed to accept multiple seeds.
- **Actual production SDK through File Vault's Vite server:** baseline first
  50.632 s / third 60.153 s; refreshed bundle first 45.027 s / third 52.719 s.
  These isolated browser contexts imported the linked SDK and called
  `AutonomiClient.connect()` then `downloadAndSave()`; they did not automate the
  File Vault UI. Each was stopped after three chunks. The final “page closed”
  message is intentional probe termination.
- **Native CLI, same seed's QUIC endpoint `207.148.94.42:10000`:** baseline
  first 25.798 s / third 28.308 s; changed first 26.264 s / third 28.067 s.
  One sample each, measured from the first CLI log including bootstrap. These
  timings are not strictly the same start boundary as the browser table.

There is still substantial cold-network variation. The initial public-map
lookup remains slow in some runs; healthy native QUIC connections and browser
WebRTC setup have different costs. The 38-second reconstruction/hash tail in
the user's original log is a separate issue and is unchanged by these fixes.
The live large-file probes intentionally stop after three chunks; no claim is
made about an improved complete 147-chunk transfer time.

## Reproduction and provenance

From the client checkout:

```sh
cd ant-core
wasm-pack build --target web --out-dir wasm-tests/pkg --release . --no-default-features --features browser-wasm,test-utils
cd browser-tests
node read-startup.mjs ../wasm-tests/pkg ../../docs/investigations/2026-09-25-read-startup/seeds.json single 3 core > startup.jsonl
```

Use `all` for all seeds or the final `sdk` argument to include the SDK-style
discarded authentication probe. The trace export is enabled only by `test-utils`;
it is absent from the production SDK. `cancelled-or-error` marks an unfinished
operation and must not be interpreted as a timeout without supporting events.
The `connected` event in core probe mode marks client construction, not network
readiness. `download_ms` is the timing boundary used above.

The auxiliary scripts beside this report retain the native CLI, local SDK demo
and File Vault SDK-path probes. The File Vault probe uses this workspace's
absolute SDK path and existing Vite server on port 5180. The demo requires the
isolated devnet on port 35000 and SDK example server on port 35174; its well-known
Anvil test key is guarded by a local-devnet check. These scripts are investigation
tools, not production entry points.

Core base: `a93360f3785bfd9eb7afd672f8c5395e2a4b25b4`, with the working-tree
changes accompanying this report. Baseline timing builds added diagnostics to
that base without the read fixes. SDK base:
`2f7ea572346b7489c096d390fefa1920e8d6c69b`.

| WASM artifact | SHA-256 |
| --- | --- |
| Original production SDK / app | `e1b5846b18d95f211d421d0586e95a53857dcbd82a2d852d9faa9ac56fe8c0d1` |
| Instrumented baseline | `9d3d513e39f9028c9191209910b62b7eab047c770124103206dc60f72cc54829` |
| Instrumented changed version | `c7221458d5d0e976d2c153954245dab3fe74036abda86ea09d5616ed4d76f2b5` |
| Refreshed production SDK / app | `c08af347ad7231e2933cacb73418191c6434090000d1a70f7d6e649b7e494574` |

The generated production bindings and `source.json` were rebuilt together via
the SDK's `npm run sync:wasm`; provenance records `dirty: true` and the exact
binary/lockfile hashes. This is a local development build, not a published
release. File Vault's built asset hash matches the refreshed SDK. Original SDK
generated files were backed up at `/tmp/ant-sdk-wasm-before-startup-fix` before
regeneration. Existing unrelated work in the SDK was preserved.

## Validation

- `cargo test -p ant-core --lib client_engine`: 128 passed.
- Generated WASM regression suite: 163 passed, including stalled speculative
  reads, the two-GET handoff bound, integrity errors, more than seven early
  misses, connected-holder preference and shared failed-dial suppression.
- Native and WASM Clippy with `-D warnings`, rustfmt and diff whitespace checks:
  passed.
- Real Chromium / seven local nodes / Anvil integration: authenticated, paid,
  recovered an interrupted upload without paying twice and downloaded matching
  bytes; passed. Node revision
  `b0263b324c418a5732d1727d7a66df4c15946559`.
- SDK `npm run check`: 185 passed; all five examples built;
  `npm pack --dry-run` passed.
- SDK all-in-one demo, driven in real Chromium against the same isolated
  seven-node/Anvil devnet: connected, paid for four records, downloaded and saved
  13,056 matching bytes through the production bundle. SDK ADR governance and
  all 52 PR-checker cases also passed.
- File Vault: 44 tests passed; production build passed; live SDK-path startup
  check and matching bundled WASM hash verified.

Compressed test logs and selected per-peer traces are retained alongside this
report. Public-network startup performance and deterministic correctness tests
provide different evidence; neither is presented as a substitute for the other.
