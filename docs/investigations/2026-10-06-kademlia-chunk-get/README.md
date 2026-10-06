# Kademlia-style chunk GET — 2026-10-06

Linear: V2-1358. Would a request that returns the chunk if the node holds it,
or its closest known peers if it doesn't, make reads faster? And is that worth
a protocol change?

## Summary

Round 1 measured where read time goes and compared the issue's combined
request with the no-protocol alternative. Round 2 tried to keep the combined
request's speed without its duplicate downloads.

- **Native reads spend most of their time finding a holder, not fetching from
  it.** Discovery is 5.1 s at p50 and transfer 0.8 s (82% of the mean). The
  closest-peers lookup runs to convergence, about 6 s, before ordinary GETs
  start.
- **Browser reads split their time evenly.** Fetching 4 MB over a WebRTC
  DataChannel takes 2.2–2.7 s at p50, about as long as discovery. The browser
  already turns lookup progress into early GETs.
- **The combined request is fast but wasteful.** An emulated combined request
  ("chunk or closer peers") cuts native p50 from 5.95 s to about 1.0 s. It
  downloads 2.5–3.3 copies of each chunk, because the round that reaches the
  close group asks several holders at once. Its speed needs no protocol change:
  the emulation is just a GET sent beside each FIND_NODE.
- **Feeding lookup progress to the early GETs needs no protocol change.**
  Native p50 drops to 1.8–2.2 s, with one copy.
- **A "do you hold it?" answer gets the combined request's speed with one
  copy.** This is the "have" policy (round 2):
  - The client asks each queried peer whether it holds the chunk, beside the
    FIND_NODE. It fetches from the first holder, and asks a second holder only
    if the first is still sending after 1.5 s.
  - A storage quote's `already_stored` flag stood in for the answer, so no node
    changed.
  - Native results in one interleaved run:

    | Policy | p50 | p90 | Copies (mean) | GETs per read |
    | --- | ---: | ---: | ---: | ---: |
    | have | 1.16 s | 2.32 s | 1.07 | 1 |
    | combined | 1.04 s | 3.16 s | 2.53 | 6 |
    | progress | 2.24 s | 5.01 s | 1.0 | 8 |

- **Whole-file reads (612 MB, four chunks in flight):**

  | Policy | Time |
  | --- | ---: |
  | baseline | 199–205 s |
  | progress | 83–91 s |
  | combined | 49–55 s |
  | capped (two GETs, closest first; no protocol change) | 54 s |
  | have | 55–57 s |

- **In the browser, no policy clearly beats today's reads.**
  - Combined pooled over three runs: 4.6 s against 5.2 s at p50, with 2.8
    copies. In round 2 every policy landed between 4.7 s and 6.8 s at p50.
  - Transfer and dead WebRTC endpoints dominate.
  - Have's 1.5 s hedge is shorter than a browser transfer, so it fetched 1.5
    copies there.
- **Offering peers before they answer ("eager") is not safe.** It doubled
  browser read time, because 36% of browser WebRTC dials fail and the dead
  endpoints took the two early-GET slots.

## Recommendation

The issue's combined request does not win. Its speed needs no protocol change,
and it downloads 2.5–3.3 copies. A smaller protocol change does win natively:

1. **Protocol: add a "do you hold this chunk?" request.**
   - `ChunkHasRequest { address }` → `ChunkHasResponse { address, held }`,
     answered from a storage existence check. The node does no lookup and no
     signing.
   - It returns no peers. The client keeps sending FIND_NODE through
     saorsa-core, so peer validation stays there and ant-core needs no lookup
     of its own.
   - Advertise it: the native user agent's node version, and a browser HELLO
     capability. Treat an unanswered probe as "unknown".
   - This is a wire change, so it needs an ant-node ADR.
2. **Client: drive reads from the lookup.**
   - Use saorsa-core's new `LookupObserver` hook to have-probe each peer as it
     is queried. Fetch from the first holder, hedged.
   - Where peers don't answer the new request, GET the two closest queried
     peers instead (capped). Capped needs only the saorsa-core hook and works
     against today's nodes, so it can ship first: native p50 1.49 s, p90
     3.48 s, 1.67 copies, whole file 54 s.
3. **Browser: use the same path.**
   - Size the hedge for DataChannel transfers (3 s or more).
   - Do not cap GETs on peers that have not connected.
   - Its main cost, transfer, needs separate work.

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
| Native run s3 (round 2) | 16:22–16:38 |
| Browser run s3 (round 2) | 20:14–20:43 |
| Whole-file runs (round 2) | 20:43–21:00 |

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

## Round 1 results

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

## Round 2: keeping the speed without the copies

The round 2 policies act on each peer the lookup queries, at the moment it is
queried, and stop at the first verified chunk:

| Policy | What it does | Protocol change |
| --- | --- | --- |
| combined | GET every queried peer (round 1). | none |
| capped | GET queried peers closest-first, two at a time. | none |
| have | Ask every queried peer whether it holds the chunk. GET from the first holder; GET a second holder if the first is still running after 1.5 s. | "have" request, emulated with a storage quote's `already_stored` |
| have_inline | Like have, but the closest queried peer of each round gets a GET instead of the question. | "have" request plus an inline flag |

Round 1's combined traces were first replayed under capped and hedged
policies (`simulate_probe_policy.py`, `estimate_capped.py`):

- **Native.** Two GETs, closest first, came out at an estimated p50 of 1.07 s
  with 1.84 copies.
- **Browser.** Every cap was much worse. Dead endpoints hold the capped slots
  for up to 10 s.

### Native (run s3, 30 reads per policy)

| Policy | p50 | p75 | p90 | max | GETs per read | Copies (mean / max) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| progress | 2.24 s | 3.60 s | 5.01 s | 9.66 s | 8 | 1.00 / 1 |
| combined | 1.04 s | 1.90 s | 3.16 s | 6.02 s | 6 | 2.53 / 9 |
| capped | 1.49 s | 2.67 s | 3.48 s | 6.74 s | 4 | 1.67 / 2 |
| have | 1.16 s | 1.89 s | 2.32 s | 3.05 s | 1 | 1.07 / 2 |
| have_inline | 1.65 s | 2.58 s | 3.53 s | 4.92 s | 3 | 1.73 / 3 |

- Have sent 5 have-probes per read, answered in 0.21 s at p50 and 0.67 s at
  p90.
- **Have_inline is worse than have, for two reasons.** An inline GET to a slow
  or dead peer holds back the holder GET until the hedge. And inline GETs that
  reach holders add copies.

### Browser (run s3, 30 reads per policy)

| Policy | p50 | p75 | p90 | max | GETs per read | Copies (mean / max) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| baseline | 5.08 s | 8.69 s | 11.55 s | 15.66 s | 14 | 1.00 / 1 |
| combined | 5.41 s | 10.34 s | 11.78 s | 27.06 s | 13 | 3.10 / 6 |
| capped | 4.74 s | 12.51 s | 16.27 s | 21.99 s | 7 | 1.53 / 2 |
| have | 5.03 s | 7.88 s | 11.71 s | 15.97 s | 2 | 1.53 / 2 |
| have_inline | 6.80 s | 9.11 s | 10.63 s | 18.88 s | 3 | 1.40 / 3 |

- **Thirty reads per policy is too few to separate these.** Combined, which
  beat the baseline in runs s1 and s2, did not here.
- **Have answers took longer in the browser.** p50 was 0.40 s and p90 2.8 s,
  and a successful GET took 2.2 s at p50. So the 1.5 s hedge fired on most
  reads, and have fetched 1.5 copies.
- **Capped's tail comes from dead endpoints.** They hold its two slots, which
  pushes p90 to 16 s.

### Whole file (four chunks in flight, two passes in opposite order)

`--concurrency 4 --drain-secs 0`, one policy per process, all 150 records:

| Policy | Pass 1 | Pass 2 |
| --- | ---: | ---: |
| baseline | 199.0 s | 204.9 s |
| progress | 83.0 s | 91.0 s |
| combined | 54.8 s | 49.4 s |
| capped | 53.9 s | 53.5 s |
| have | 57.3 s | 55.3 s |

- **The lookup-driven policies all finish within a few seconds of each other.**
  They are about 3.7 times faster than the baseline and 1.6 times faster than
  progress.
- **Duplicates cost bytes here, not wall time.** On this link, combined's
  extra copies did not cost wall time. They are still 2.5–3.3 times the bytes,
  for the client and for the nodes that send them.

## Protocol notes

- **"No separate GET round trip after the lookup."**
  - Confirmed as the dominant native cost: baseline discovery is 5.1 s at p50.
  - It is removed by acting on each peer as the lookup queries it. Combined
    and capped do that with no protocol change.
- **"No waiting for the lookup's final rounds when the chunk exists."**
  - Confirmed. The lookup-driven policies almost never wait for the lookup to
    finish.
- **Duplicate downloads.** This is the real cost of returning the chunk
  inline.
  - Without some way to learn who holds a chunk before it arrives, a client
    either races holders and gets copies (combined), or caps its GETs and
    waits on slow or dead peers (capped).
  - A "have" answer removes that choice: have reaches combined's speed with
    one copy.
- **Why the request should not return peers.**
  - saorsa-core's FIND_NODE validates returned peers in private code:
    - `trusted_find_node_response_nodes`
    - `validate_transport_address_records`
    - the LAN filter
    - `lookup_candidate_dial_plan_is_exhausted`
  - A peer-returning ant-protocol message would need ant-core to run its own
    native lookup and duplicate that validation.
  - Sending a small have request beside FIND_NODE keeps discovery in
    saorsa-core. The `LookupObserver` hook is all the client needs.
- **Capability.**
  - Native QUIC has no capability exchange, and an old node silently drops an
    unknown `ChunkMessageBody` variant.
  - The node's user agent (`node/0.21.0`) is known once a peer is connected,
    and can gate the request.
  - A have-probe that gets no answer is only "unknown". The client still has
    the peer's FIND_NODE answer, and can fall back to a capped GET.
  - The browser would use a HELLO capability, like `pointer_protocol`.
- **Absence.** Deciding that a chunk is missing still needs a completed lookup
  and a majority of `NotFound`s, which `held: false` answers can count as. No
  policy here changes that rule.
- **Node cost.** A have answer is one file existence check and a few bytes.
  The emulation's quotes are heavier: they are signed. Their answer times are
  therefore an upper bound for a real have answer.

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
- **Round 2 ran later than round 1.** It has 30 reads per policy, and
  compares policies only within each run. Native p50 for progress was 1.83 s
  in round 1 and 2.24 s in round 2.
- **Have's hedge (1.5 s) and capped's limit (2) were not tuned.**

## Reproduction

The bench needs saorsa-core with `LookupObserver`. ant-client's root
`Cargo.toml` patches it to a sibling checkout on the V2-1358 branch.

```sh
# Native strategy runs
cargo run --release --example read-strategy-bench -- \
  --addresses docs/investigations/2026-10-06-kademlia-chunk-get/chunks.txt \
  --out run-s3.jsonl --seed 3 --strategies progress,combined,capped,have,have_inline

# Native whole-file run for one policy
cargo run --release --example read-strategy-bench -- \
  --addresses docs/investigations/2026-10-06-kademlia-chunk-get/chunks.txt \
  --out tp-have.jsonl --seed 10 --strategies have --concurrency 4 --drain-secs 0

# Browser strategy runs (Playwright with Chromium installed)
wasm-pack build --target web --release \
  --out-dir docs/investigations/2026-10-06-kademlia-chunk-get/browser/pkg ant-core \
  --no-default-features --features browser-wasm,test-utils
cd docs/investigations/2026-10-06-kademlia-chunk-get/browser
PLAYWRIGHT=<path to playwright/index.mjs> node run.mjs ../chunks.txt run-s3.jsonl 3 5 30 baseline,combined,capped,have,have_inline

# Analysis
python3 analyze_strategies.py results/native-strategies-run-s1.jsonl.gz results/native-strategies-run-s2.jsonl.gz
python3 split.py results/native-strategies-run-s1.jsonl.gz results/native-strategies-run-s2.jsonl.gz
python3 analyze_native.py results/native-download-diagnostics.jsonl.gz
python3 simulate_probe_policy.py results/native-strategies-run-s1.jsonl.gz results/native-strategies-run-s2.jsonl.gz
```

Raw traces are in [`results/`](results):

- The full-download diagnostics have node network addresses removed.
- The strategy traces contain peer ids and timings only.
