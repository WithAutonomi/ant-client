# Normal-path download diagnostics design

## Status

Implementation design for the V2-903 investigation. This is temporary, runtime-gated diagnostic instrumentation rather than a change to download policy.

## Requirement

Capture one JSON Lines record for each normal-path chunk fetch attempt while preserving the existing early-success behaviour, retry policy, adaptive concurrency, stdout, and default resource use.

The diagnostic runner is independent of version A/B testing. It should run beside the existing PROD-DL-01 runner in the same region and provider so the network vantage is not changed.

## Runtime interface

`ant file download ... --download-diagnostics <PATH>`

- Omitted: the existing path and output remain unchanged; no diagnostic channel or file is created.
- Present: the CLI opens a sidecar JSONL file and passes an optional bounded diagnostics sender through the normal file/chunk download path.
- Diagnostic output never replaces or contaminates normal stdout or `--json` output.
- Records are streamed as attempts finish so a later process or file-download failure does not discard earlier evidence.

## Record schema

Each record contains:

| Field | Meaning |
|---|---|
| `schema_version` | Stable schema discriminator, initially `1` |
| `timestamp` | UTC time when the attempt completed |
| `file_attempt` | Outer file/deferred-retry attempt number |
| `chunk_index` / `chunk_address` | Chunk identity within the file |
| `sweep` | Initial or internal retry sweep |
| `peer_attempt` | Peer attempt number within the sweep |
| `lookup_duration_ms` | Closest-peer DHT lookup duration; emitted on the first attempt associated with that lookup |
| `expected_peer` | Peer selected by the DHT lookup |
| `source_peer` | Peer identified by the received protocol response |
| `transport_source` | Actual response event transport MultiAddr, when available |
| `route` | `direct`, `relay`, `lan`, `unverified`, or `unknown`, classified from the actual transport source against typed DHT addresses |
| `response_elapsed_ms` | Elapsed time until the complete response was reassembled and delivered |
| `ttfb_ms` | `null` until the protocol exposes a first-byte/first-frame event |
| `ttfb_available` / `ttfb_unavailable_reason` | Explicitly prevents complete-response latency being presented as TTFB |
| `bytes` | Valid returned chunk bytes; otherwise `0` |
| `outcome` | `found`, `not_found`, `timeout`, `network_error`, `protocol_error`, `cache_hit`, `lookup_error`, or `exhausted` |
| `error` | Bounded diagnostic error category/detail; no secrets |

A cache hit is a chunk-level record without a source peer or transport route. Lookup failures and exhausted peer sets are also explicit records rather than silent gaps.

## Transport classification

`saorsa-core` supplies a small public route-classification API on `P2PNode`. It classifies the actual `P2PEvent::Message.transport_source` against the peer's typed DHT addresses. It must not infer route type from address-list position or from the expected destination address.

## TTFB limitation

The current protocol event is emitted only after complete message reassembly. Therefore this branch can measure complete-response latency, not true network time-to-first-byte. True TTFB requires a new first-frame/streaming event in `ant-protocol` or the transport layer and is deliberately left unavailable here.

## Safety and bounds

- The normal early-return path remains normal; unlike `--all-peers`, diagnostics do not query every close-group peer after success.
- The optional channel is bounded. A slow diagnostic writer must not create unbounded memory growth; its failure is surfaced without altering the fetched data result.
- Error strings are bounded and diagnostics contain no credentials.
- Existing public methods delegate to the diagnostic-capable implementation with diagnostics disabled.

## Verification

- Route classification unit tests cover direct, relay, LAN, unverified, and unknown addresses.
- Normal-path tests cover cache hit, first-peer success, retry sweep, and exhausted/error outcomes.
- A disabled-diagnostics regression test confirms the existing path does not allocate or emit records.
- JSON schema/serialization tests pin field names and the explicit unavailable-TTFB representation.
- Run `cargo fmt --check`, focused tests, and `cargo check` in both repositories.
- Independent reviewers check specification compliance, concurrency/back-pressure behaviour, route correctness, and compatibility before either branch is pushed.
