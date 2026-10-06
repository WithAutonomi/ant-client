# Kademlia-style chunk GET — 2026-10-06

Linear: V2-1358. Would a request that returns the chunk if the node holds it,
or its closest known peers if it doesn't, make reads faster? And is that worth
a protocol change?

## Summary

- **Native reads spend most of their time finding a holder, not fetching from
  it.** Discovery is 5.1 s at p50 and transfer 0.8 s (82% of the mean). The
  closest-peers lookup runs to convergence, about 6 s, before ordinary GETs
  start. Early GETs to already-connected peers found the chunk in only 23 of
  88 reads.
- **Browser reads split their time evenly.** Fetching 4 MB over a WebRTC
  DataChannel takes 2.7 s at p50, about as long as discovery (2.4 s). The
  browser already turns lookup progress into early GETs.
- **An emulated combined request is the fastest option, but it downloads
  several copies.**
  - Natively it cuts p50 from 5.95 s to 0.97 s and p90 from 13.0 s to 1.9 s.
    On average it downloads 3.3 copies of each chunk (at most 11), because the
    round that reaches the close group queries several holders at once.
  - In the browser it cuts p50 only from 5.2 s to 4.2 s and p90 from 13.5 s to
    9.0 s, with 2.75 copies. The copies slow each other down: one successful
    GET slows from 2.7 s to 4.5 s.
- **Most of the native gain needs no protocol change.** If lookup progress
  feeds the existing early-GET path, as the browser already does, native p50 is
  1.8 s and p90 4.9 s, with one copy.
- **Offering peers as they are queried ("eager") is not safe.** It helps a
  little natively (1.5 s p50, 4.2 s p90) but doubles browser read time (10.3 s
  p50, 25 s p90). Endpoints that have not answered yet take the two early-GET
  slots, and 36% of browser WebRTC dials fail.
- **A protocol change is not needed to get combined-request latency.** The
  emulation is just a GET sent beside each FIND_NODE. What only a protocol
  change can add is a cheap "I hold it" answer, so the client downloads fewer
  copies. Estimated from the traces:

  | Variant | Native p50 | Native p90 | Copies | Browser p50 |
  | --- | ---: | ---: | ---: | ---: |
  | Only the closest peer per round inlines the chunk | 1.17 s | 3.0 s | 1.6 | 5.0 s |
  | "Have it + closer peers", then one fetch | ≤ 1.56 s | ≤ 5.7 s | 1 | ≤ 5.6 s |

  The browser baseline is 5.2 s.

**Recommendation: the combined request does not win. Do not add the protocol
message.**

1. **Native: give the lookup progress hints the browser already gets.** Add
   the small saorsa-core `LookupObserver` hook prototyped here. Offer each
   FIND_NODE responder, and each already-connected peer it returns, to
   `retrieve_progressive`. Measured: p50 5.95 s → 1.83 s, p90 13.0 s → 4.9 s,
   max 29.2 s → 12.8 s, one copy, 6 GETs per read instead of 15. Do not offer
   cold queried peers ("eager").
2. **Browser: nothing from this issue.** Its remaining time is DataChannel
   transfer and dead WebRTC endpoints. Neither a combined message nor more
   hints addresses those.
3. **Revisit "have it + closer peers" only if native tails stay well above the
   estimates above** once step 1 ships. Its estimated gain over step 1 is
   0.3–0.7 s at p50. In exchange it needs:
   - a new message
   - a native lookup outside saorsa-core
   - a capability fallback for old nodes

## Method

Everything ran against production mainnet from one workstation, starting from
the bundled seeds:

| Run | Window (UTC) |
| --- | --- |
| Native full download | 14:02–14:06 |
| Native run s1 | 14:15–14:35 |
| Browser run s1 | 14:35–15:02 |
| Native run s2 | 15:03–15:18 |
| Browser run s2 | 15:18–15:49 |

The connected nodes reported `node/0.21.0` (937) and `node/0.20.0` (23).

The test data is the 612 MB video
`134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889` used by
the earlier read investigations. Its 150 records (147 content, 3 metadata) are
listed in [`chunks.txt`](chunks.txt).

1. **Where read time goes today.** One native `ant file download …
   --download-diagnostics` of the whole file, at its default adaptive
   concurrency, using ant-client `9858af5`.
2. **Strategy comparison.**
   - Each record is read once with one strategy. Strategies are interleaved in
     shuffled blocks, so all of them see the same network and the same warming
     client.
   - Reads run one at a time and bypass the chunk cache.
   - Native uses the `read-strategy-bench` example, one process per run
     (`--seed 1`, `--seed 2`).
   - Browser uses headless Chromium with `benchChunkRead`: five fresh
     `BrowserNetworkClient` sessions of 30 reads per run.
   - Every GET is recorded with its start, end, outcome and verified bytes.
     After a combined read, in-flight GETs are drained for up to 15 s so the
     duplicate copies they carry are counted.

Strategies (code in `ant-core/src/data/client/read_bench.rs` and
`ant-core/src/browser/wasm_transport/read_bench.rs`):

| Strategy | What it does | Protocol change |
| --- | --- | --- |
| baseline | Today's `chunk_get`: `retrieve_progressive`. Early GETs go to peers that were connected before the lookup (1 GET, a second after 1 s). Ordinary GETs, in XOR order, follow the completed lookup. On the browser, lookup progress also feeds hints. | — |
| progress | Native only. Baseline, plus each FIND_NODE responder, and each connected peer it returns, is offered as an early-GET hint while the lookup runs. This is the browser baseline's behaviour. | none |
| eager | Progress, plus each peer is offered when the lookup queries it. Still at most two GETs at once. | none |
| combined | Every peer the lookup queries also gets a GET at that moment, and the read stops at the first verified chunk. This emulates "chunk or closer peers" with no node change. | none for the emulation |

Both native strategies that observe the lookup use a new saorsa-core hook,
`LookupObserver` (`on_query`, `on_response`). It is passed to
`DhtNetworkManager::find_closest_nodes_observed`. The hook is on the
`mickvandijke/v2-1358-…` branch of saorsa-core and has a unit test. Lookup
parameters are unchanged:

| | alpha | grace per round |
| --- | ---: | ---: |
| Native | 3 | 5 s |
| Browser | 6 | 5 s |

Two further combined variants are **estimated offline** from the combined
traces:

- **inline-closest.** In each lookup round only the peer closest to the
  target is asked to inline the chunk. Other holders answer "have it".
- **have+fetch.** Every holder answers "have it + closer peers", and the
  client fetches from the first holder whose answer arrives. The estimate adds
  that holder's FIND_NODE round trip, including any dial, to its GET. It is an
  upper bound. It can only be computed when the holder's FIND_NODE answer
  arrived before the lookup was cancelled (49 of 87 native reads).

## Results

### Where native read time goes today (full download)

150 chunk fetches at the CLI's adaptive fetch concurrency. Total time 242 s.

| Per chunk | p50 | p90 | p99 | max |
| --- | ---: | ---: | ---: | ---: |
| First send → verified chunk | 5.4 s | 13.4 s | 19.0 s | 29.7 s |
| Closest-peers lookup | 5.2 s | 9.8 s | 15.2 s | 15.4 s |
| Lookup end → chunk (ordinary GETs) | 1.1 s | 4.8 s | 12.6 s | 21.7 s |
| One successful 4 MB GET | 1.0 s | 2.7 s | 5.9 s | 7.0 s |
| One `NotFound` GET | 0.15 s | 0.45 s | 0.84 s | 2.5 s |

- Of 2,315 GETs, 2,159 were early speculative `NotFound`s, about 20 per
  chunk.
- Early GETs found the chunk for only 32 of 150 chunks. The rest waited for
  the lookup.

### Native strategy comparison

Two runs pooled (s1: 50 reads per strategy, s2: 37–38). Every read found its
chunk.

| Strategy | Reads | p50 | p75 | p90 | max | GETs per read | Copies downloaded (mean / max) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| baseline | 88 | 5.95 s | 9.10 s | 13.04 s | 29.22 s | 15 | 1.0 / 1 |
| progress | 88 | 1.83 s | 3.05 s | 4.90 s | 12.84 s | 6 | 1.0 / 1 |
| eager (s2 only) | 37 | 1.50 s | 2.66 s | 4.22 s | 7.73 s | 3 | 1.0 / 1 |
| combined | 87 | 0.97 s | 1.56 s | 1.89 s | 5.52 s | 6 | 3.3 / 11 |
| inline-closest (est.) | 84 | 1.17 s | 1.76 s | 3.05 s | 6.12 s | — | 1.6 / 5 |
| have+fetch (est., upper) | 49 | 1.56 s | 2.45 s | 5.73 s | 8.13 s | — | 1.0 / 1 |

Each read splits into discovery (until the holder that served the chunk was
asked) and transfer (that GET):

| Strategy | Discovery p50 | Discovery p90 | Transfer p50 | Transfer p90 | Discovery share of mean |
| --- | ---: | ---: | ---: | ---: | ---: |
| baseline | 5.08 s | 10.06 s | 0.83 s | 2.33 s | 82% |
| progress | 1.03 s | 3.18 s | 0.81 s | 2.00 s | 60% |
| eager | 0.48 s | 1.88 s | 1.00 s | 2.56 s | 37% |
| combined | 0.28 s | 0.77 s | 0.64 s | 1.58 s | 34% |

- **Which round found the chunk.** In combined reads it was round 1 for 16
  reads, round 2 for 62 and round 3 for 9.
- **Why combined downloads several copies.** With alpha 3, the second round
  is usually where the lookup reaches the close group, and it queries up to
  three holders at once. Every holder then sends a full copy.
- **Combined transfer looks faster than it is.** Its transfer is the fastest
  of several parallel copies. One successful GET took 1.11 s at p50 under
  combined, against 0.81–1.0 s for the strategies that fetch one copy.

### Browser

Two runs, each of five fresh client sessions of 30 reads. s1 compared
baseline and combined (75 reads each). s2 compared baseline, eager and
combined (50 reads each). Every read found its chunk. Across both runs, 36% of
WebRTC Direct dials failed (1,775 of 4,967).

| Strategy | Reads | p50 | p75 | p90 | max | GETs per read | Copies downloaded (mean / max) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| baseline | 125 | 5.17 s | 8.44 s | 13.48 s | 21.19 s | 16 | 1.0 / 1 |
| eager (s2 only) | 50 | 10.29 s | 17.40 s | 24.99 s | 53.13 s | 11 | 1.0 / 1 |
| combined | 125 | 4.18 s | 6.48 s | 9.00 s | 31.37 s | 12 | 2.75 / 6 |
| inline-closest (est.) | 123 | 5.02 s | 7.90 s | 10.29 s | 35.26 s | — | 1.3 / 3 |
| have+fetch (est., upper) | 121 | 5.61 s | 8.12 s | 13.54 s | 35.26 s | — | 1.0 / 1 |

| Strategy | Discovery p50 | Discovery p90 | Transfer p50 | Transfer p90 | Discovery share of mean |
| --- | ---: | ---: | ---: | ---: | ---: |
| baseline | 2.35 s | 7.50 s | 2.71 s | 5.38 s | 52% |
| eager | 2.61 s | 15.33 s | 3.64 s | 9.71 s | 50% |
| combined | 0.67 s | 5.46 s | 2.56 s | 5.26 s | 42% |

- **The baseline already uses lookup progress.** Early GETs won all 125
  baseline reads. Transfer is half the time: one 4 MB GET takes 2.7 s at p50,
  against 0.8 s natively.
- **Eager is harmful in the browser.**
  - It offers peers before they have answered, so endpoints that may never
    connect take the two early-GET slots.
  - In 20 of 50 eager reads, a GET failed with a network error before the
    winning GET started. Each such failure held a slot for 5.8 s on average.
  - This is the mechanism the [read-startup
    investigation](../2026-09-25-read-startup/README.md) removed by seeding only
    authenticated peers.
  - QUIC fails fast natively (1.25 s per direct attempt), so the native run
    saw only 2 network errors in 37 eager reads.
- **Combined helps the browser for a different reason.** Each queried peer's
  GET rides on the connection the lookup is already opening, and there is no
  two-slot cap, so a dead endpoint delays only its own probe. That cuts
  discovery to 0.67 s at p50. But transfer still dominates, and parallel
  copies slow each successful GET from 2.7 s to 4.5 s, so the total improves
  only 19% at p50 and 33% at p90.
- **The duplicate-free variants gain less still in the browser.** The
  estimates are 5.0 s p50 (inline-closest) and at most 5.6 s (have+fetch),
  against a 5.2 s baseline.

## What a protocol change would and would not buy

- **"No separate GET round trip after the lookup."**
  - Confirmed as the dominant native cost: baseline discovery is 5.1 s at p50.
  - Removing most of that wait does not need a new message. Feeding FIND_NODE
    responders to the early-GET path (progress) cuts it to 1.0 s. A GET sent
    beside each FIND_NODE (combined) cuts it to 0.3 s, also without a new
    message.
- **"No waiting for the lookup's final rounds when the chunk exists."**
  - Confirmed. Combined, progress and eager reads almost never wait for the
    lookup to finish (5 of 212 reads), against 67 of 88 baseline reads.
  - The same is true of every strategy that observes the lookup, including
    those with no protocol change.
- **Duplicate downloads.** These are the real cost of returning the value
  inline:
  - Natively, 3.3 copies per chunk on average, or 13 MB to read a 4 MB chunk.
  - Under load, the copies compete for the client's downlink and the holders'
    uplinks. In the browser this already shows: one successful GET slowed from
    2.65 s to 4.6 s.
  - A protocol change could carry a "have it" flag instead of the chunk. Only
    then could the client keep the round trip and download one copy. The
    estimates above put that variant at about 1.2–1.6 s p50 natively, against
    progress's measured 1.8 s.

If the protocol change is pursued later, these points from the issue still
apply:

- **Message size and validation.** saorsa-core's FIND_NODE runs on the DHT
  topic, which caps a message at 64 KiB, so it cannot carry a 4 MB chunk. A
  chunk-carrying lookup response would have to be a new ant-protocol
  `ChunkMessageBody` variant, and ant-core would need its own native lookup.
  That lookup would have to validate returned peers as FIND_NODE does, which
  means exposing these private saorsa-core functions:
  - `trusted_find_node_response_nodes`
  - `validate_transport_address_records`
  - the LAN filter
  - `lookup_candidate_dial_plan_is_exhausted`

  `client_routing::compute_winner` is already public.
- **Capability.** Native QUIC has no capability exchange. An old node silently
  drops an unknown `ChunkMessageBody` variant, so a client would wait for its
  timeout. That is the `QuoteRequestV2` probe-and-fallback problem again. The
  node's user agent (`node/0.21.0`) is available for connected peers and could
  gate the request. The browser would use a HELLO capability, like
  `pointer_protocol`.
- **Absence.** Deciding that a chunk is missing would still need a completed
  lookup and a majority of `NotFound`s. No strategy here changes that rule.

## Caveats

- **Emulation fidelity.**
  - Combined sends two messages per peer, a FIND_NODE and a GET, where a real
    combined request sends one. The extra `NotFound` is small, and both share
    one connection and one deduplicated dial, so latency should match. Bytes
    received match apart from the extra `NotFound` frames.
  - inline-closest and have+fetch are estimates, not runs.
- **Reads were sequential.** The CLI downloads several chunks at once. The
  full-download baseline (fetch concurrency 4, p50 5.4 s) agrees with the bench
  baseline (5.95 s). Under concurrency, combined's extra copies would contend
  more than they did here.
- **Only existing chunks were read.** The absence path is not measured. It is
  unchanged by every strategy.
- **One vantage point, one afternoon.** The native differences are large
  compared with run-to-run variation: baseline p50 was 5.2 s in s1 and 6.7 s
  in s2. The browser differences between baseline and combined are smaller,
  and should be read as direction, not size.
- **Eager has one run per platform** (37 native reads, 50 browser reads).

## Reproduction

The bench needs saorsa-core with `LookupObserver`. ant-client's root
`Cargo.toml` patches it to a sibling checkout on the V2-1358 branch.

```sh
# Native strategy runs
cargo run --release --example read-strategy-bench -- \
  --addresses docs/investigations/2026-10-06-kademlia-chunk-get/chunks.txt \
  --out run-s1.jsonl --seed 1 --strategies baseline,progress,eager,combined

# Browser strategy runs (Playwright with Chromium installed)
wasm-pack build --target web --release \
  --out-dir docs/investigations/2026-10-06-kademlia-chunk-get/browser/pkg ant-core \
  --no-default-features --features browser-wasm,test-utils
cd docs/investigations/2026-10-06-kademlia-chunk-get/browser
PLAYWRIGHT=<path to playwright/index.mjs> node run.mjs ../chunks.txt run-s1.jsonl 1 5 30 baseline,eager,combined

# Analysis
python3 analyze_strategies.py results/native-strategies-run-s1.jsonl.gz results/native-strategies-run-s2.jsonl.gz
python3 split.py results/native-strategies-run-s1.jsonl.gz results/native-strategies-run-s2.jsonl.gz
python3 analyze_native.py results/native-download-diagnostics.jsonl.gz
```

Raw traces are in [`results/`](results):

- The full-download diagnostics have node network addresses removed.
- The strategy traces contain peer ids and timings only.
