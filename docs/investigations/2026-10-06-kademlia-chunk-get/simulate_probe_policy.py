"""Replay combined traces under a capped, hedged probe policy.

Each queried peer becomes probe-eligible when the lookup queries it. At most
`base` probes run at once; one more may start (up to `cap`) once every running
probe has run for `hedge` ms. The closest eligible peer goes first. A probe's
duration and outcome come from the trace, where every queried peer was probed
at query time; a later start is assumed to take as long (conservative: its dial
is usually done by then)."""
import gzip, json, sys
def open_any(p): return gzip.open(p, 'rt') if p.endswith('.gz') else open(p)
def pct(xs, p):
    xs = sorted(xs); k = max(0, min(len(xs)-1, -(-p*len(xs)//100)-1)); return xs[k]

def replay(t, base, cap, hedge):
    target = int(t['address'], 16)
    peers = [(a['started_ms'], int(a['peer'], 16) ^ target, a['completed_ms'] - a['started_ms'], a['outcome'] == 'found')
             for a in t['attempts'] if a['round'] is not None]
    peers.sort()
    probed, running, started_found = set(), [], 0
    now = 0
    while True:
        # Launch as many probes as the policy allows at `now`.
        while True:
            ready = [p for i, p in enumerate(peers) if p[0] <= now and i not in probed]
            if not ready: break
            allowed = len(running) < base or (len(running) < cap and all(now - s >= hedge for s, _, _ in running))
            if not allowed: break
            i = peers.index(min(ready, key=lambda p: p[1]))
            probed.add(i)
            q, _, dur, found = peers[i]
            running.append((now, now + dur, found))
            started_found += found
        events = [end for _, end, _ in running]
        events += [p[0] for i, p in enumerate(peers) if i not in probed and p[0] > now]
        if len(running) < cap and running:
            events += [max(s for s, _, _ in running) + hedge] if all(True for _ in running) else []
        events = [e for e in events if e > now]
        if not events: return None, started_found
        now = min(events)
        done = [r for r in running if r[1] <= now]
        running = [r for r in running if r[1] > now]
        if any(found for _, _, found in done):
            return now, started_found

reads = [json.loads(l) for p in sys.argv[1:] for l in open_any(p) if l.strip()]
reads = [r['trace'] for r in reads if r.get('trace') and r['strategy'] == 'combined' and r['trace']['found']]
print(f"{'policy':28s} {'n':>4s} {'unk':>4s} {'p50':>6s} {'p75':>6s} {'p90':>6s} {'max':>6s} {'copies':>7s} {'max':>4s}")
for base, cap, hedge in [(1,1,10**9),(1,2,500),(1,2,1000),(1,2,2000),(1,3,500),(1,3,1000),(2,2,0),(2,3,1000),(99,99,0)]:
    times, copies, unknown = [], [], 0
    for t in reads:
        done_at, c = replay(t, base, cap, hedge)
        if done_at is None: unknown += 1; continue
        times.append(done_at); copies.append(c)
    name = f"base={base} cap={cap} hedge={hedge if hedge < 10**9 else 'none'}"
    print(f"{name:28s} {len(times):4d} {unknown:4d} {pct(times,50):6.0f} {pct(times,75):6.0f} {pct(times,90):6.0f} {max(times):6.0f} {sum(copies)/len(copies):7.2f} {max(copies):4d}")
