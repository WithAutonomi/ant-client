# Browser lookup parallelism — 2026-09-29

Test file: `134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889`
(612 MB, 147 records), the video from the
[read-startup investigation](../2026-09-25-read-startup/README.md). All runs
used headless Chromium against mainnet, starting from the bundled seeds. The
code was ant-client `fix/browser-media-streaming` (`e85cc57`) plus the lookup
change under test.

## What a lookup waits on

`test-utils` traces of three 20-peer `findClosest` lookups:

| Dial outcome | Count | Duration |
| --- | ---: | --- |
| Authenticated | 106 | p50 1.9 s, p90 5.3 s, max 8.6 s |
| `WebRTC DataChannel opening timed out` | 106 | 10.0 s each |

About half of the WebRTC Direct endpoints these lookups dialled were
unreachable from a browser. In the cold-read runs below, 232 of 701 dials
failed. A lookup round queries `alpha` peers, takes the
first answer, and waits up to `ITERATION_GRACE_TIMEOUT_SECS` (5 s) for the rest.
With the Kademlia default `alpha` of 3, most rounds include an unreachable
peer and pay most of the grace period. Read lookups need several rounds.

## Cold record reads

`run-reads.mjs` opens the video in a fresh page. It then reads one byte from
each of 12 records spread across the file (indices 4, 16, …, 136). No record
is cached, so every read discovers that record's holders. Variants run
interleaved in rounds, so network conditions change for all of them alike.
Raw data is in [`results/`](results).

| Variant | Reads | p50 | p75 | p90 | Max | Mean | Over 10 s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| alpha 3, grace 5 s (current) | 36 | 4.1 s | 13.7 s | 29.2 s | 120.0 s | 11.5 s | 12 |
| alpha 6, grace 5 s | 36 | 3.5 s | 7.0 s | 17.2 s | 21.1 s | 6.1 s | 6 |
| alpha 10, grace 5 s | 36 | 3.3 s | 6.0 s | 7.5 s | 33.6 s | 4.5 s | 2 |
| alpha 3, grace 2 s | 24 | 6.1 s | 23.8 s | 31.2 s | 55.7 s | 12.9 s | 10 |
| alpha 6, grace 2 s | 24 | 3.1 s | 7.2 s | 20.8 s | 81.9 s | 8.2 s | 4 |

The 120-second baseline read hit the bench's read timeout. A shorter grace
period did not help: it drops slow but live responders, so lookups converge
less well. A wider round did help.

## Media playback

`stream-seek.mjs` plays the video through the SDK example. It seeks after 30
seconds of playback and then watches playback. `stream-summary.mjs` reports the
first frame after connecting, the seek resume time, and time spent waiting
after the seek.

| alpha | First frame | Seek resume | Waiting after seek |
| ---: | --- | --- | --- |
| 3 | 17.1 / 62.3 / 39.2 s | 12.3 / 23.4 / 8.1 s | 1.8 / 0 / 0 s |
| 6 | 16.9 / 14.2 / 19.0 s | 3.7 / 17.6 / 24.0 s | 0 / 1.9 / 0.1 s |
| 10 | 20.1 / 24.5 / 20.5 / 18.3 / 19.3 / 22.4 s | 11.3 / 35.8 / 11.2 / 6.4 / 4.0 / 11.0 s | 12.6 / 0 / 18.9 / 0 / 17.6 / 21.2 s |

Playback read-ahead runs several lookups at once, so alpha 10 opens dozens of
WebRTC connections together against a 64-connection pool. It waited 12 to 21
seconds after four of six seeks. Alpha 6 never waited more than two seconds.

## Decision

Browser lookups use an `alpha` of 6. For cold reads, that roughly halves the
mean read time and the p90 relative to 3. It gave the steadiest first frame
during playback without the contention seen at 10. The grace period stays at the
protocol's 5 seconds. Seek resume time remains variable at every alpha and is
bound by how quickly a new region's holders are found.

## Reproduce

From the ant-client root, build `test-utils` bindings for each variant into a
scratch copy of this directory, then serve and run it:

```sh
wasm-pack build --target web --release --out-dir <scratch>/pkg-v0 ant-core \
  --no-default-features --features browser-wasm,test-utils
PLAYWRIGHT=<path to @playwright/test> VARIANTS=pkg-v0,pkg-v3 ROUNDS=3 \
  node <scratch>/run-reads.mjs
```

For playback, sync the production bindings into the browser SDK
(`ANT_CLIENT_DIR=<this checkout> npm run sync:wasm && npm run build`). Then run
`SEEK_AFTER=30 SEEK_TO=420 WATCH_SECONDS=80 node stream-seek.mjs >
run.log` and summarize with `node stream-summary.mjs run.log`. `alpha-seek-*`
in the playback summary are the first three alpha 10 runs.
