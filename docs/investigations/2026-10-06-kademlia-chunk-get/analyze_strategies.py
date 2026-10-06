"""Compare read strategies from read-strategy-bench / browser bench JSONL."""
import gzip, json, sys, statistics as st
from collections import defaultdict

def open_any(path):
    return gzip.open(path, 'rt') if path.endswith('.gz') else open(path)


def pct(xs, p):
    xs = sorted(xs)
    if not xs: return float('nan')
    k = max(0, min(len(xs) - 1, -(-p * len(xs) // 100) - 1))
    return xs[k]

def row(name, xs, unit=""):
    xs = [x for x in xs if x is not None]
    if not xs: return f"  {name:34s} n=0"
    return (f"  {name:34s} n={len(xs):3d} p50={pct(xs,50):8.0f} p75={pct(xs,75):8.0f} p90={pct(xs,90):8.0f} "
            f"p99={pct(xs,99):8.0f} max={max(xs):8.0f} mean={st.mean(xs):8.0f}{unit}")

def row_f(name, xs):
    xs = [x for x in xs if x is not None]
    if not xs: return f"  {name:34s} n=0"
    return (f"  {name:34s} n={len(xs):3d} p50={pct(xs,50):6.2f} p90={pct(xs,90):6.2f} max={max(xs):6.2f} mean={st.mean(xs):6.2f}")

lines = []
for path in sys.argv[1:]:
    lines += [json.loads(l) for l in open_any(path) if l.strip()]
by = defaultdict(list)
timeouts = defaultdict(int)
for l in lines:
    if l.get('timed_out') or not l.get('trace'):
        timeouts[l['strategy']] += 1
        continue
    by[l['strategy']].append(l['trace'])

for strategy in ['baseline', 'progress', 'eager', 'combined']:
    ts = by.get(strategy)
    if not ts: continue
    found = [t for t in ts if t['found']]
    print(f"== {strategy}: reads={len(ts)} found={len(found)} not_found_or_error={len(ts)-len(found)} timed_out={timeouts[strategy]}")
    print(row("time to chunk (ms)", [t['total_ms'] for t in found]))
    print(row("lookup completed at (ms)", [t['lookup_ms'] for t in ts if t['lookup_ms'] is not None]))
    print(f"  lookup completed before chunk:    {sum(1 for t in found if t['lookup_ms'] is not None and t['lookup_ms'] <= t['total_ms'])}/{len(found)}")
    print(row("GETs sent per read", [len(t['attempts']) for t in ts]))
    print(row("GETs before chunk per read", [sum(1 for a in t['attempts'] if a['started_ms'] <= t['total_ms']) for t in found]))
    sizes = {}
    for t in found:
        for a in t['attempts']:
            if a['outcome'] == 'found': sizes[t['address']] = a['bytes']
    print(row_f("chunk copies downloaded", [sum(a['bytes'] for a in t['attempts']) / sizes[t['address']] for t in found if sizes.get(t['address'])]))
    print(row("bytes downloaded per read (KiB)", [sum(a['bytes'] for a in t['attempts']) / 1024 for t in found]))
    outcomes = defaultdict(int)
    for t in ts:
        for a in t['attempts']: outcomes[a['outcome']] += 1
    print("  GET outcomes:", dict(outcomes))
    if strategy != 'combined':
        early = sum(1 for t in found if any(a['outcome'] == 'found' and a['early'] and a['completed_ms'] <= t['total_ms'] + 1 for a in t['attempts']))
        print(f"  won by early GET:                 {early}/{len(found)}")
        print(row("successful GET duration (ms)", [a['completed_ms'] - a['started_ms'] for t in found for a in t['attempts'] if a['outcome'] == 'found']))
    else:
        rounds = defaultdict(int)
        for t in found: rounds[t['found_round']] += 1
        print("  found in lookup round:", dict(sorted(rounds.items(), key=lambda kv: (kv[0] is None, kv[0] or 0))))
        print(row("successful GET duration (ms)", [a['completed_ms'] - a['started_ms'] for t in found for a in t['attempts'] if a['outcome'] == 'found']))
        # "Have + closer peers, then one fetch": the holder's FIND_NODE answer
        # stands in for a small "have" reply; one more round trip precedes the
        # transfer. Upper bound: first holder answer + that holder's full GET.
        a2, a2_lo = [], []
        for t in found:
            answers = {a['peer']: a for a in t['answers']}
            best = None
            for a in t['attempts']:
                ans = answers.get(a['peer'])
                if a['outcome'] != 'found' or not ans: continue
                est = a['completed_ms'] + (ans['answered_ms'] - ans['queried_ms'])
                best = est if best is None else min(best, est)
            if best is not None:
                a2.append(best); a2_lo.append(t['total_ms'])
        print(row("have+fetch estimate (ms, upper)", a2))
        print(f"  (have+fetch lower bound is the combined time itself; n={len(a2)})")
        print(row("FIND_NODE answer RTT (ms)", [a['answered_ms'] - a['queried_ms'] for t in ts for a in t['answers']]))

def xor(a_hex, b_hex):
    return int(a_hex, 16) ^ int(b_hex, 16)

# Offline variants of the combined request, from the Combined traces:
#  - have+fetch: every holder answers "have + peers"; the client fetches from
#    the first holder whose answer arrives (one copy, one extra round trip).
#  - inline-closest: per lookup round, only the peer closest to the target is
#    asked to inline the chunk; other holders answer "have + peers".
ts = [t for t in by.get('combined', []) if t['found']]
if ts:
    hf_t, hf_n, ic_t, ic_copies, ic_n = [], 0, [], [], 0
    for t in ts:
        answers = {a['peer']: a for a in t['answers']}
        rounds = defaultdict(list)
        for a in t['attempts']:
            if a['round'] is not None:
                rounds[a['round']].append(a)
        inline = {min(r, key=lambda a: xor(a['peer'], t['address']))['peer'] for r in rounds.values()}
        have_fetch = []
        for a in t['attempts']:
            ans = answers.get(a['peer'])
            if a['outcome'] == 'found' and ans:
                have_fetch.append(a['completed_ms'] + (ans['answered_ms'] - ans['queried_ms']))
        if have_fetch:
            hf_t.append(min(have_fetch)); hf_n += 1
        options = [a['completed_ms'] for a in t['attempts'] if a['outcome'] == 'found' and a['peer'] in inline]
        options += have_fetch
        if options:
            best = min(options)
            ic_t.append(best); ic_n += 1
            inline_copies = sum(1 for a in t['attempts'] if a['outcome'] == 'found' and a['peer'] in inline and a['started_ms'] <= best)
            won_inline = any(a['outcome'] == 'found' and a['peer'] in inline and a['completed_ms'] == best for a in t['attempts'])
            ic_copies.append(inline_copies + (0 if won_inline else 1))
    print("== combined variants (offline, from combined traces)")
    print(row("inline-all (measured)", [t['total_ms'] for t in ts]))
    print(row("have+fetch (upper estimate)", hf_t))
    print(row("inline-closest-per-round (est.)", ic_t))
    print(row_f("inline-closest copies (est.)", ic_copies))
