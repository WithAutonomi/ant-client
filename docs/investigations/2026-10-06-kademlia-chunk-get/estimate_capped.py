"""Offline: GET only the k closest queried peers per lookup round (client-only)."""
import gzip, json, sys
from collections import defaultdict
def open_any(p): return gzip.open(p, 'rt') if p.endswith('.gz') else open(p)
def pct(xs, p):
    xs = sorted(xs); k = max(0, min(len(xs)-1, -(-p*len(xs)//100)-1)); return xs[k]
reads = [json.loads(l) for p in sys.argv[1:] for l in open_any(p) if l.strip()]
reads = [r['trace'] for r in reads if r.get('trace') and r['strategy'] == 'combined' and r['trace']['found']]
for k in [1, 2, 3, 99]:
    times, copies, unknown = [], [], 0
    for t in reads:
        target = int(t['address'], 16)
        rounds = defaultdict(list)
        for a in t['attempts']:
            if a['round'] is not None: rounds[a['round']].append(a)
        probed = []
        for r in sorted(rounds):
            probed += sorted(rounds[r], key=lambda a: int(a['peer'], 16) ^ target)[:k]
        found = [a['completed_ms'] for a in probed if a['outcome'] == 'found']
        if not found: unknown += 1; continue
        best = min(found)
        times.append(best)
        copies.append(sum(1 for a in probed if a['outcome'] == 'found' and a['started_ms'] <= best))
    print(f"k={k:2d}: n={len(times)} unknown={unknown} p50={pct(times,50)} p75={pct(times,75)} p90={pct(times,90)} max={max(times)} copies mean={sum(copies)/len(copies):.2f} max={max(copies)}")
