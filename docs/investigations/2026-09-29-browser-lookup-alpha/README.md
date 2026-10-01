# Browser lookup parallelism — 2026-09-29

Test file: `134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889`
(612 MB, 147 records), the video from the
[read-startup investigation](../2026-09-25-read-startup/README.md). All runs
used headless Chromium against mainnet, starting from the bundled seeds. The
code was ant-client `fix/browser-media-streaming` (`e85cc57`) plus the lookup
change under test. That revision predates the read-ahead bounds #209 gained
before it merged: it kept up to eight fetches in flight through the shared chunk
cache, where the merged read-ahead keeps at most five. The playback comparison
below should be repeated on the merged read-ahead.

## What a lookup waits on

`test-utils` traces of three 20-peer `findClosest` lookups:

| Dial outcome | Count | Duration |
| --- | ---: | --- |
| Authenticated | 106 | p50 1.9 s, p90 5.3 s, max 8.6 s |
| `WebRTC DataChannel opening timed out` | 106 | 10.0 s each |

About half of the WebRTC Direct endpoints these lookups dialled were
unreachable from a browser. In the second cold-read run below, 232 of the alpha 3
build's 701 dials failed. A lookup round queries `alpha` peers, takes the
first answer, and waits up to `ITERATION_GRACE_TIMEOUT_SECS` (5 s) for the rest.
A round with an unreachable peer pays most of that grace, and most rounds have
one at either width: if a third of peers are unreachable, about 70% of rounds of
three and 91% of rounds of six. A wider round waits about as long but returns
more answers, so a lookup needs fewer rounds. Read lookups need several rounds.

## Cold record reads

`run-reads.mjs` opens the video in a fresh page. It then reads one byte from
each of 12 records spread across the file (indices 4, 16, …, 136). No record
is cached, so every read discovers that record's holders. Variants run
interleaved in rounds, so network conditions change for all of them alike.
Raw data is in [`results/`](results), where each variant is named by its build:

| Build | alpha | Grace |
| --- | ---: | ---: |
| `pkg-v0` | 3 | 5 s |
| `pkg-v1` | 3 | 2 s |
| `pkg-v2` | 6 | 2 s |
| `pkg-v3` | 6 | 5 s |
| `pkg-v4` | 10 | 5 s |

The grace 2 s rows come from the first run, `reads-1790670357573`, whose own
alpha 3, grace 5 s baseline had a mean of 12.1 s over 24 reads. The other rows
come from the second run, `reads-1790671278653`. The first run records every
dial as failed, so its dial counts are not used.

| Variant | Reads | p50 | p75 | p90 | Max | Mean | Over 10 s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| alpha 3, grace 5 s (current) | 36 | 4.1 s | 13.1 s | 29.2 s | 120.0 s | 11.5 s | 12 |
| alpha 6, grace 5 s | 36 | 3.4 s | 6.6 s | 17.2 s | 21.1 s | 6.1 s | 6 |
| alpha 10, grace 5 s | 36 | 3.1 s | 6.0 s | 7.5 s | 33.6 s | 4.5 s | 2 |
| alpha 3, grace 2 s | 24 | 5.3 s | 17.6 s | 31.2 s | 55.7 s | 12.9 s | 10 |
| alpha 6, grace 2 s | 24 | 3.1 s | 5.5 s | 20.8 s | 81.9 s | 8.2 s | 4 |

p50 is the median; p75 and p90 are nearest-rank percentiles.

The 120-second baseline read hit the bench's read timeout. `reads.js` does not
cancel a timed-out read, so its lookups kept running alongside that page's
later reads and may have slowed them. A shorter grace
period did not help: it drops slow but live responders, so lookups converge
less well. A wider round did help.

## Media playback

`stream-seek.mjs` plays the video through the SDK example. It seeks 30 seconds
after starting playback, to 420, 540 and 660 seconds in the first, second and
third run of each alpha, and stops watching 80 seconds after starting.
`stream-summary.mjs` reports the first frame after connecting (before the
seek), the seek resume time, time spent waiting after the seek, and how long
playback was watched after it resumed.

| alpha | First frame | Seek resume | Waiting after seek | Watched after seek |
| ---: | --- | --- | --- | --- |
| 3 | 17.1 / none / 39.2 s | 12.3 / 23.4 / 8.1 s | 1.8+ / 0 / 0 s | 38 / 27 / 42 s |
| 6 | 16.9 / 14.2 / 19.0 s | 3.7 / 17.6 / 24.0 s | 0 / 1.9 / 0.1+ s | 46 / 32 / 26 s |
| 10 | 20.1 / 24.5 / 20.5 / 18.3 / 19.3 / 22.4 s | 11.3 / 35.8 / 11.2 / 6.4 / 4.0 / 11.0 s | 12.6 / 0 / 18.9 / 0 / 17.6 / 21.2 s | 39 / 14 / 39 / 44 / 46 / 39 s |

"none" is a run that had not started playing when it seeked. "+" marks a wait
still going when the log ended, so its length is unknown.

Playback read-ahead ran several lookups at once, so alpha 10 opened dozens of
WebRTC connections together against a 64-connection pool. It waited 12 to 21
seconds after four of six seeks. Alpha 6 waited at most two seconds in the
time watched, but its third run was waiting when the log ended, 26 seconds
after resuming. With three runs at six and 14 to 46 seconds watched after each
seek, these runs suggest that ten stalls more, but they do not separate six
from ten. Seek resume did not improve with alpha: its mean was 14.6 s at 3,
15.1 s at 6 and 13.3 s at 10. The run logs are in `results/stream-*.log.gz`
and `results/alpha-seek-*.log.gz`, and `results/streaming-summary.txt` is their
summary.

## Upload quoting

`run-uploads.mjs` uploads 8 MB of random content, which is four records, in
single-node payment mode. It stops at the payment request and declines to pay,
so nothing is stored or paid for. This covers the upload's lookups and quote
collection. Two uploads ran per fresh page, with the variants interleaved over
three rounds.

| alpha | Uploads | Time to payment request | Median | Mean | Dials ok / failed / refused |
| ---: | ---: | --- | ---: | ---: | --- |
| 3 | 6 | 131, 142, 146, 156, 173, 184 s | 151 s | 155 s | 1215 / 649 / 418 |
| 6 | 6 | 91, 108, 119, 131, 136, 137 s | 125 s | 120 s | 1712 / 816 / 426 |

Every upload reached the payment request with quotes for all four records.
Alpha 6 was about 23% faster, at the cost of about 40% more authenticated
connections. Quoting remains slow at either alpha, and needs its own investigation.

The bench records every `connect` trace that does not end connected as a failed
dial. "Refused" counts those that ended at 0 ms, before any dial: the
failed-endpoint cache turns a known-dead endpoint away at once. They are
counted apart from dials that failed.

## Second machine

The runs above used a 32-core Apple M3 Ultra on a home connection. The same
alpha 3 and 6 builds (identical `.wasm` SHA-256), pages and runners were run on
a DigitalOcean `s-2vcpu-4gb-amd` droplet in `ams3`, with Ubuntu 24.04 and
Playwright 1.63.0 Chromium. Raw data is in `results/droplet-*`.

| Machine | alpha | Cold read p50 | p90 | Mean | Reads over 10 s | Upload mean (range) |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| M3 Ultra, 32 cores | 3 | 4.1 s | 29.2 s | 11.5 s | 12 of 36 | 155 s (131–184) |
| M3 Ultra, 32 cores | 6 | 3.4 s | 17.2 s | 6.1 s | 6 of 36 | 120 s (91–137) |
| Droplet, 2 vCPU | 3 | 4.6 s | 13.9 s | 8.0 s | 9 of 36 | 161 s (132–203) |
| Droplet, 2 vCPU | 6 | 3.9 s | 14.6 s | 6.2 s | 6 of 36 | 105 s (87–125) |

Alpha 6 also helped on the 2-vCPU machine. The extra concurrency cost no CPU
headroom there: upload quoting improved by 35%, against 23% on the M3 Ultra.
Cold reads gained less on the droplet, and their p90 rose slightly, because its
alpha 3 tail was already shorter, which fits its data-centre network rather than
its CPU. No read or upload failed on the droplet. On the M3 Ultra every upload
reached the payment request, and one alpha 3 read timed out.

## Decision

Browser lookups use an `alpha` of 6. Relative to 3, it roughly halved the
cold-read mean on the M3 Ultra and cut the p90 by two fifths. On the droplet it
cut the mean by about a fifth and left the p90 about the same. It shortened
upload quoting by a quarter to a third. Ten read faster still but opens more
connections, and it stalled after more seeks during playback. The playback runs
are too few and too short to separate six from ten, so six is the conservative
choice: it keeps most of ten's cold-read gain with fewer connections. Its first
frames, 14 to 19 seconds, were the most consistent: at 3 one run had not started
playing by its seek, and at 10 they took 18 to 25 seconds. The grace period
stays at the protocol's 5 seconds. Seek resume time did not change with alpha
and is bound by how quickly a new region's holders are found.

## Reproduce

From the ant-client root, build `test-utils` bindings for each variant into a
scratch copy of this directory, then serve and run it. A variant sets
`BROWSER_LOOKUP_ALPHA` and, for the grace variants, the
`ITERATION_GRACE_TIMEOUT_SECS` duration in the browser lookup's `query_batch`:

```sh
wasm-pack build --target web --release --out-dir <scratch>/pkg-v0 ant-core \
  --no-default-features --features browser-wasm,test-utils
export PLAYWRIGHT=<path to @playwright/test>
# Cold reads: the first run, then the second. The droplet ran VARIANTS=pkg-v0,pkg-v3 ROUNDS=3.
VARIANTS=pkg-v0,pkg-v1,pkg-v2 ROUNDS=2 node <scratch>/run-reads.mjs
VARIANTS=pkg-v0,pkg-v3,pkg-v4 ROUNDS=3 node <scratch>/run-reads.mjs
# Upload quoting, two uploads per page, on both machines.
VARIANTS=pkg-v0,pkg-v3 ROUNDS=3 UPLOADS=2 node <scratch>/run-uploads.mjs
```

For playback, sync the production bindings into the browser SDK
(`ANT_CLIENT_DIR=<this checkout> npm run sync:wasm && npm run build`). Then run
each alpha three times, with `SEEK_TO` set to 420, 540 and 660 in turn:
`SEEK_AFTER=30 SEEK_TO=420 WATCH_SECONDS=80 node stream-seek.mjs >
stream-a6-1.log`. `SEEK_AFTER` must be a multiple of 10, because the script
checks it every 10 seconds; any other value never seeks. Summarize with
`node stream-summary.mjs results/stream-*.log.gz results/alpha-seek-*.log.gz`,
which also reads plain logs. `alpha-seek-*` are the first three alpha 10 runs.
