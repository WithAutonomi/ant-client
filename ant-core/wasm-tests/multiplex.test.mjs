import assert from 'node:assert/strict';
import test from 'node:test';
import { test_multiplex_requests, test_cancelled_read_reservation, test_stale_rpc_admission, test_admitted_hello_never_redials, test_closed_session_error, test_draining_pool_capacity, test_multiplex_puts } from './pkg/ant_core.js';
import { mockWebRtc } from './mock-webrtc.mjs';
const object = v => v instanceof Map ? Object.fromEntries([...v].map(([k,x]) => [k, object(x)])) : Array.isArray(v) ? v.map(object) : v;

for (const cancel of [false, true]) test(`multiplexed replies finish out of order and ${cancel ? 'cancelled' : 'slow'} work keeps its slot until drained`, async () => {
  let issued = 0, active = 0, peak = 0;
  const rtc = mockWebRtc([{
    multiplex: true,
    respond(_ch, method) { if (method === 'get_chunk') { issued++; peak = Math.max(peak, ++active); } },
    delay(method) { return method === 'get_chunk' ? (issued === 1 ? 180 : 35) : 0; },
    delivered(_ch, method) { if (method === 'get_chunk') active--; },
  }]);
  const {results, reuse} = object(await test_multiplex_requests(rtc.endpoints[0], 9, cancel));
  assert.equal(results[0].result, cancel ? 'cancelled' : 'ok');
  assert.ok(results.slice(1).every(r => r.result === 'ok'), JSON.stringify(results));
  assert.ok(results[1].ms < 150, JSON.stringify(results));
  if (!cancel) assert.ok(results[0].ms >= 170);
  assert.equal(peak, 4);
  assert.equal(active, 0);
  assert.equal(reuse, true);
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.connections[0].channels.length, 1);
  assert.equal(rtc.requests.filter(r => r.method === 'hello').length, 1);
});

test('older nodes keep one request in flight and cancelled callers still reuse the session', async () => {
  let active = 0, peak = 0;
  const rtc = mockWebRtc([{
    respond(_ch, method) { if (method === 'get_chunk') peak = Math.max(peak, ++active); },
    delay: method => method === 'get_chunk' ? 45 : 0,
    delivered(_ch, method) { if (method === 'get_chunk') active--; },
  }]);
  const {results, reuse} = object(await test_multiplex_requests(rtc.endpoints[0], 5, true));
  assert.equal(results[0].result, 'cancelled');
  assert.ok(results.slice(1).every(r => r.result === 'ok'));
  assert.equal(peak, 1);
  assert.equal(reuse, true);
  assert.equal(rtc.connections.length, 1);
});

test('cancelled physical reads retain their memory reservation through draining and decoding', async () => {
  const rtc = mockWebRtc([{ multiplex: true, delay: method => method === 'get_chunk' ? 100 : 0 }]);
  await test_cancelled_read_reservation(rtc.endpoints[0]);
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.requests.filter(r => r.method === 'hello').length, 1);
});

test('an old admission cannot send into a replacement authenticated session', async () => {
  const rtc = mockWebRtc([{ multiplex: true }]);
  await test_stale_rpc_admission(rtc.endpoints[0]);
  assert.equal(rtc.connections.length, 2);
  assert.equal(rtc.requests.filter(r => r.method === 'find_node').length, 1);
});

test('an admitted session that closes underneath fails without redialling', async () => {
  const rtc = mockWebRtc([{ multiplex: true }]);
  await test_admitted_hello_never_redials(rtc.endpoints[0]);
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.requests.filter(r => r.method === 'hello').length, 1);
});

test('a pooled RPC whose session closed reports it without a reconnect instruction', async () => {
  const rtc = mockWebRtc([{ multiplex: true }]);
  assert.equal(await test_closed_session_error(rtc.endpoints[0]), 'WebRTC session closed');
  assert.equal(rtc.requests.filter(r => r.method === 'find_node').length, 0);
});

test('pool eviction waits for abandoned replies and wakes when draining completes', async () => {
  const rtc = mockWebRtc([{ multiplex: true, delay: method => method === 'find_node' ? 100 : 0 }, {}]);
  await test_draining_pool_capacity(rtc.endpoints);
  assert.equal(rtc.connections.length, 2);
  assert.ok(rtc.connections.every(c => c.closed));
});

test('PUTs retain per-lane serialization on multiplex-capable nodes', async () => {
  let active = 0, peak = 0;
  const rtc = mockWebRtc([{
    multiplex: true,
    respond(_ch, method) { if (method === 'put_chunk') peak = Math.max(peak, ++active); },
    delay: method => method === 'put_chunk' ? 35 : 0,
    delivered(_ch, method) { if (method === 'put_chunk') active--; },
  }]);
  assert.deepEqual(await test_multiplex_puts(rtc.endpoints[0]), Array(6).fill(true));
  assert.equal(peak, 1);
  assert.equal(rtc.connections.length, 1);
  assert.equal(rtc.requests.filter(r => r.method === 'hello').length, 1);
});
