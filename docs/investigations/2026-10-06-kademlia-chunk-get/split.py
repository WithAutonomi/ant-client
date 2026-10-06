"""Split each read into discovery (until the serving holder was asked) and transfer (that GET)."""
import gzip, json, sys
from collections import defaultdict

def open_any(path):
    return gzip.open(path, 'rt') if path.endswith('.gz') else open(path)

def pct(xs, p):
    xs = sorted(xs); k = max(0, min(len(xs)-1, -(-p*len(xs)//100)-1)); return xs[k]
lines = [json.loads(l) for path in sys.argv[1:] for l in open_any(path) if l.strip()]
by = defaultdict(list)
for l in lines:
    t = l.get('trace')
    if not t or not t['found']: continue
    win = min((a for a in t['attempts'] if a['outcome'] == 'found'), key=lambda a: a['completed_ms'])
    by[l['strategy']].append((t['total_ms'], win['started_ms'], win['completed_ms'] - win['started_ms']))
print(f"{'strategy':10s} {'n':>4s} | {'total p50':>9s} {'p90':>7s} | {'discovery p50':>13s} {'p90':>7s} | {'transfer p50':>12s} {'p90':>7s} | discovery share of mean")
for s in ['baseline', 'progress', 'eager', 'combined']:
    rs = by.get(s)
    if not rs: continue
    tot = [r[0] for r in rs]; disc = [r[1] for r in rs]; xfer = [r[2] for r in rs]
    print(f"{s:10s} {len(rs):4d} | {pct(tot,50):9d} {pct(tot,90):7d} | {pct(disc,50):13d} {pct(disc,90):7d} | {pct(xfer,50):12d} {pct(xfer,90):7d} | {sum(disc)/sum(tot):.0%}")
