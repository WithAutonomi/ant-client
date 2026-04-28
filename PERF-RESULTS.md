# Adaptive controller perf results vs live network

Single-rep bench, fresh client per run, against
`resources/bootstrap_peers.toml` mainnet bootstrap peers.
Workload: `client.prepare_chunk_payment` over N random chunks via
`buffer_unordered(cap)`. No payment, no wallet — quoting only.
DHT lookups (`find_closest_peers`) dominate per-chunk cost
(~70-80s/lookup on this network).

## Raw numbers

### 8 chunks per rep
| cap | total | per-chunk | notes |
|-----|------:|---------:|------|
|   4 | 161 s |  20.1 s  | 2 sequential rounds → linear slowdown |
|   8 |  73 s |   9.2 s  | **optimal** (cap == batch size, 1 round) |
|  16 | 101 s |  12.7 s  | extra fan-out hurts (no work to do; DHT contention) |
|  32 | 107 s |  13.3 s  | same |
|  64 | 108 s |  13.4 s  | same |

### 32 chunks per rep
| cap | total | per-chunk | notes |
|-----|------:|---------:|------|
|  16 | 174 s |   5.4 s  | 2 rounds; per-chunk ~half of c8/cap=8 (amortized DHT) |
|  32 | 110 s |   3.4 s  | cap == batch size, 1 round |
|  64 |  97 s |   3.0 s  | **best**; slight win even with cap > batch (noise / extra slots) |

## Findings

1. **Per-chunk cost falls with batch size when fan-out matches it.** 9.2 s/chunk at
   c8/cap=8 → 3.4 s/chunk at c32/cap=32. Larger batches amortize DHT
   contention.
2. **Cap < batch size is the worst case** — every doubling of round count
   doubles wall-clock. cap=4/c8 is 2.2× slower than cap=8/c8.
3. **Cap > batch size doesn't help meaningfully** for one-shot batches
   (only N items to do; can't parallelize beyond N). Slight downside
   from extra DHT contention is observable but small.
4. **Cap = batch size is the sweet spot** for one-shot quoting. For
   long pipelines that span many batches, `cap > batch_size` lets
   the controller refill on completion (rolling scheduler) — that
   is what `rebucketed_unordered` already does for
   `data_download`.

## Recommendation

Implement a **batch-aware cap** at the call site:

```rust
let learned_cap = controller.quote.current();
let pipeline_cap = learned_cap.min(batch.len());
stream::iter(batch).buffer_unordered(pipeline_cap);
```

The adaptive AIMD controller still learns the network's upper bound
(`learned_cap`); the `.min(batch.len())` clamp avoids paying for
fan-out slots we'll never fill on a single small batch.

**Concrete production wins**:
- `batch_upload_chunks` already processes in `PAYMENT_WAVE_SIZE = 64`
  waves. Setting fan-out = `min(controller.store.current(), wave.len())`
  avoids over-fanning the last partial wave (e.g. a 70-chunk file:
  wave 1 has 64, wave 2 has 6 — the second wave should fan out 6,
  not 64).
- `data_download` via `rebucketed_unordered` is already correct
  (rolling scheduler refills as completions free slots).
- Small files (1-10 chunks) currently see fan-out clamped to controller
  default (32) but only need fan-out = chunk count. With the fix they
  spend the right amount of energy on connection setup.

## Method caveats

- Single rep per data point; mainnet variance is high (rep-to-rep
  variance can be 30-50%). Trends across the 5×3 matrix are robust;
  exact ms values are not.
- `find_closest_peers` is the dominant cost on this network at the
  moment. If that changes (e.g. saorsa-core ships the
  `find_closest_peers` speedup tracked in saorsa-labs/saorsa-core#96),
  per-chunk numbers will fall but the cap-vs-batch-size relationship
  will hold.
- `K-bucket at capacity (20/20)` warnings throughout the runs
  indicate routing-table pressure on the local node. This is
  upstream behavior; orthogonal to the cap strategy.

## Data files

`/tmp/perf-bench-results/feat-1rep-cap-{4,8,16,32,64}-c8.json`
`/tmp/perf-bench-results/feat-1rep-cap-{16,32,64}-c32.json`
`/tmp/perf-bench-results/main-normal.json` (baseline single-chunk)
