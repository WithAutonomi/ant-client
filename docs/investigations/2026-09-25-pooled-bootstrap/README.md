# Retained bootstrap connections and early discovery — 2026-09-25

This follows the [read-startup fixes](../2026-09-25-read-startup/README.md) and the
seven bundled WebRTC seeds. The previous SDK authenticated a temporary
`BrowserNodeClient`, closed it, and constructed a separate `BrowserNetworkClient`.
Profile connections tried seeds sequentially. The Rust network client's cold
lookup then waited for every seed to authenticate before running discovery.

## Implemented behavior

The SDK now creates one `BrowserNetworkClient` and calls its additive `connect`
binding. That Rust client authenticates through the pool used for subsequent
discovery and transfers. It starts up to four bootstrap attempts concurrently,
returns the first seed passing peer authentication, protocol-capability and
expected-payment checks, and retains successful sessions. Remaining attempts
continue within the same bound. Each control/data channel still owns its own
authenticated RPC state; the WebRTC association is reused.

Cold discovery starts with seeds already authenticated. Later authenticated
seeds are offered to progressive reads. If the first walk has insufficient
results, subsequent ready seeds provide fallback within the overall lookup
budget. A seed already covered by the completed walk does not trigger another
walk just because its background bootstrap task completed later. Both the
15-second cached and 30-second seeded lookup budgets remain bounded; the latter
now also covers waiting for bootstrap results.

The shared iterative lookup engine, chunk read scheduler, quorum rules and
content verification are unchanged by this follow-up. Native source paths were
not modified. The standalone WASM `BrowserNodeClient` remains available for
low-level direct-peer use, but is no longer part of normal SDK startup.

The expected bootstrap payment policy is fixed before startup. Rejected seeds
cannot re-enter through discovery or ordinary GET fallback in the same pool.
SDK cancellation closes the pool immediately and frees the WASM allocation once
its pending async method has settled. Closing the pool cancels both active and
queued bootstrap attempts.

## Measurements

The exact file was
`134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889`.
Both variants used all seven bundled seeds and default adaptive concurrency.
Three baseline and three changed SDK runs were alternated, with fresh Chromium
contexts, and stopped after three verified content chunks. The probe served
each SDK's built ESM/WASM assets through its own local HTTP server. No wallet or
EVM RPC was used in the public-file probes.

`results.json` contains the measured milestones and medians; `traces/` retains
all six JSONL logs. Total startup includes WASM initialization and the SDK's
connection call. Download-only timing begins immediately before
`downloadAndSave()`. Small public-network samples establish observed behavior,
not a latency guarantee or full-file transfer improvement.

| Median (three runs each) | Previous SDK | Pooled startup |
| --- | ---: | ---: |
| SDK connection | 5.272 s | 2.109 s |
| Public DataMap after download starts | 9.776 s | 4.427 s |
| First chunk after download starts | 23.238 s | 16.499 s |
| First chunk including connection | 28.505 s | 18.591 s |
| Third chunk including connection | 31.721 s | 24.744 s |

Reproduce from the client checkout after building the SDK:

```sh
node ant-core/browser-tests/sdk-startup.mjs ../ant-client-browser-sdk/dist 3 candidate > candidate.jsonl
```

For a comparison, preserve the previous SDK `dist` directory before rebuilding
and alternate it with the changed one. The local baseline used here is retained
at `target/pooled-bootstrap-baseline-sdk` (ignored build output).

File Vault's build was refreshed and its WASM checksum verified. Its existing
UI still passes an explicit single seed; that path benefits from retaining its
connection. The all-seven timing comparison uses the SDK's default profile,
which is where concurrent seed selection applies.

## Sources and checks

- Core base: `6396669c97db67fe54304830e4d4a49d4974445e`, plus this working-tree
  implementation. The amended Proposed ADR-0004 documents connection ownership
  and fallback. Changed implementation fingerprints are in `source-sha256.json`.
- SDK base: `f9ff6d7`, plus the accompanying SDK changes and amended Proposed
  ADR-0002. The generated bindings and provenance were rebuilt together with
  `npm run sync:wasm`; `dirty: true` identifies this development build.
- Baseline production WASM SHA-256:
  `6bac53390f8709cf2f824933aa0420c930994328e6a80e7ccfc1b69b21f3f417`.
- Changed production WASM SHA-256:
  `bec8ca3b33e0f74de1e24df49f47d71189a16012117243f7f77f0e392006b23c`.
- Generated WASM tests: 169 passed. New tests cover reuse, first-ready discovery,
  late-seed fallback, the four-attempt bound, cancellation, immutable bootstrap
  trust and wrong-network rejection. Existing upload query-count and quorum
  regressions also pass.
- Shared Rust client engine: 128 passed. Native and WASM Clippy with warnings
  denied, rustfmt, whitespace checks and ADR governance passed.
- SDK build/types/tests: 186 passed, including delayed async cleanup after
  cancellation. All five examples built; `npm pack --dry-run` passed.
- Real Chromium drove the SDK all-in-one demo against seven isolated nodes and
  Anvil. It connected using an explicit devnet endpoint, paid for four records,
  uploaded, downloaded and saved 13,056 matching bytes with no page errors.
  Node revision: `b0263b324c418a5732d1727d7a66df4c15946559`. Task-owned servers
  and nodes were shut down afterward.
- File Vault build and all 44 tests passed.

Compressed check logs accompany the live traces. No changes were pushed or
published; these measurements exercise the local working-tree artifacts.
