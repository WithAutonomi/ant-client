"""Per-chunk split of native read time from --download-diagnostics JSONL."""
import gzip, json, sys, statistics as st
from collections import defaultdict

def open_any(path):
    return gzip.open(path, 'rt') if path.endswith('.gz') else open(path)


def pct(xs, p):
    if not xs: return float('nan')
    xs = sorted(xs); k = max(0, min(len(xs)-1, round(p/100*len(xs)+0.5)-1))
    return xs[k]

def summarize(name, xs):
    xs = [x for x in xs if x is not None]
    if not xs: print(f"  {name:38s} n=0"); return
    print(f"  {name:38s} n={len(xs):4d} p50={pct(xs,50):7.0f} p90={pct(xs,90):7.0f} p99={pct(xs,99):7.0f} max={max(xs):7.0f} mean={st.mean(xs):7.0f}")

for path in sys.argv[1:]:
    recs = [json.loads(l) for l in open_any(path)]
    by_chunk = defaultdict(list)
    for r in recs:
        by_chunk[(r['file_attempt'], r['chunk_address'])].append(r)
    total, lookup, after_lookup, early_win_time = [], [], [], []
    attempts, early_attempts, notfound, timeouts, bytes_dl, dup = [], [], [], [], [], 0
    won_by = defaultdict(int); outcomes = defaultdict(int)
    get_success_ms, get_nf_ms = [], []
    for key, rs in by_chunk.items():
        rs.sort(key=lambda r: r['request_started_unix_ms'] or 0)
        for r in rs:
            outcomes[(r['sweep'], r['outcome'])] += 1
            if r['outcome'] == 'found': get_success_ms.append(r['response_elapsed_ms'])
            if r['outcome'] == 'not_found': get_nf_ms.append(r['response_elapsed_ms'])
        starts = [r['request_started_unix_ms'] for r in rs if r['request_started_unix_ms']]
        found = [r for r in rs if r['outcome'] == 'found']
        if not starts or not found: continue
        t0 = min(starts)
        first = min(found, key=lambda r: r['request_completed_unix_ms'])
        total.append(first['request_completed_unix_ms'] - t0)
        won_by[first['sweep']] += 1
        lk = [r['lookup_duration_ms'] for r in rs if r['lookup_duration_ms'] is not None]
        if lk:
            lookup.append(lk[0])
            init = [r for r in rs if r['sweep'] != 'early']
            if init and first['sweep'] != 'early':
                lookup_end = min(r['request_started_unix_ms'] for r in init)
                after_lookup.append(first['request_completed_unix_ms'] - lookup_end)
        attempts.append(len(rs)); early_attempts.append(sum(1 for r in rs if r['sweep']=='early'))
        bytes_dl.append(sum(r['bytes'] or 0 for r in found)); dup += max(0, len(found)-1)
    print(f"== {path}: {len(recs)} attempts, {len(by_chunk)} chunk fetches")
    summarize("chunk total (first send -> chunk) ms", total)
    summarize("lookup_duration_ms (when lookup ran)", lookup)
    summarize("after lookup -> chunk ms (non-early)", after_lookup)
    summarize("GET success response ms", get_success_ms)
    summarize("GET not_found response ms", get_nf_ms)
    summarize("attempts per chunk", attempts)
    summarize("early attempts per chunk", early_attempts)
    print("  won by sweep:", dict(won_by), " duplicate successes:", dup)
    print("  outcomes:", dict(sorted(outcomes.items())))
