// posthog: the upstream outbox / HTTP unit cases as behaviour, against a demo module configured
// to send to a mock PostHog on 127.0.0.1:4203 (the host is runtime config, no build flag).
import assert from 'node:assert/strict';
import { call, callErr, done, newIdentity, procErr, sleep, sql, test, unwrap, until } from './lib.mjs';
import { startMock } from './mock-http.mjs';

const PORT = Number(process.env.POSTHOG_MOCK_PORT ?? 4203);
const HOSTURL = `http://127.0.0.1:${PORT}`;
const opt = (v) => (v === undefined ? { none: [] } : { some: v });
const unopt = (o) => (o[0] === 0 ? o[1] : undefined);
const STATUS = ['Queued', 'Processing', 'Delivered', 'Failed', 'Rejected'];
const SOURCE = ['Direct', 'Flush', 'FeatureFlag'];

// --- the mock PostHog --------------------------------------------------------------------
let batchMode = 'ok'; // ok | 503 | 401 | 408 | drop | big
const flags = {
  flags: {
    enabled: { enabled: true },
    disabled: { enabled: false },
    experiment: { enabled: true, variant: 'control' },
    invalid: { enabled: 'true' },
  },
  requestId: 'x'.repeat(3000),
};
const mock = await startMock(PORT, (r) => {
  if (r.method === 'POST' && r.path === '/batch') {
    if (batchMode === 'drop') return { destroy: true };
    if (batchMode === 'big') return { status: 503, body: 'y'.repeat(3000) };
    if (batchMode !== 'ok') return { status: Number(batchMode), body: `mock ${batchMode}` };
    return { status: 200, json: { status: 1 } };
  }
  if (r.method === 'POST' && r.path === '/flags') {
    if (r.query.v !== '2') return { status: 400, body: 'need v=2' };
    if (JSON.parse(r.body).distinct_id === 'down') return { status: 500, body: 'flags exploded' };
    return { status: 200, json: flags };
  }
  return { status: 404, body: 'no route' };
});
const batches = () => mock.requests.filter((r) => r.path === '/batch').map((r) => JSON.parse(r.body));

// --- helpers -----------------------------------------------------------------------------
const outbox = async () => (await sql('SELECT * FROM posthog_outbox')).map((r) => ({ ...r, status: STATUS[r.status[0]] }));
const outboxRow = async (id) => (await outbox()).find((r) => r.outbox_id === id);
const stats = async () => {
  const [s] = await sql('SELECT * FROM posthog_delivery_stats');
  return s ? [s.pending, s.delivered, s.failed] : [0, 0, 0];
};
let u = 0;
const uniq = (p) => `${p}_${Date.now()}_${u++}`;

await call('demo_pause_scheduler', []);

await test('config: host and key validated before the admin check; status; admin gate', async () => {
  const stranger = await newIdentity();
  assert.equal(await procErr('set_posthog_config', ['ftp://x', 'phc_1'], stranger), 'posthog.invalid_host');
  assert.equal(await procErr('set_posthog_config', ['https://', 'phc_1']), 'posthog.invalid_host');
  assert.equal(await procErr('set_posthog_config', ['https:///path', 'phc_1']), 'posthog.invalid_host');
  assert.equal(await procErr('set_posthog_config', ['https://us.i.posthog.com', '   ']), 'posthog.invalid_project_api_key');
  assert.equal(await procErr('set_posthog_config', ['https://us.i.posthog.com', 'phc_1'], stranger), 'posthog.not_authorized');
  assert.equal(await procErr('get_posthog_config_status', [], stranger), 'posthog.not_authorized');
  assert.deepEqual(unwrap(await call('get_posthog_config_status', [])), [false, [1, []], 0]);
  assert.equal(await procErr('posthog_capture_now', ['u1', 'ev', opt()]), 'posthog.config_missing');
  assert.equal(await procErr('posthog_flush_outbox', [10]), 'posthog.config_missing');
  unwrap(await call('set_posthog_config', [`  ${HOSTURL.toUpperCase().replace('HTTP', 'Http')}/// `, '  phc_test_key ']));
  const s = unwrap(await call('get_posthog_config_status', []));
  assert.deepEqual(s, [true, [0, `${HOSTURL.toUpperCase().replace('HTTP', 'Http')}`], 12]);
  unwrap(await call('set_posthog_config', [HOSTURL, 'phc_test_key']));
});

await test('admins: add, remove, never the last', async () => {
  const other = await newIdentity();
  assert.equal(await procErr('posthog_add_admin_identity', [{ __identity__: '0x' + other.identity }], other), 'posthog.not_authorized');
  unwrap(await call('posthog_add_admin_identity', [{ __identity__: '0x' + other.identity }]));
  unwrap(await call('get_posthog_config_status', [], other));
  unwrap(await call('posthog_remove_admin_identity', [{ __identity__: '0x' + other.identity }], other));
  assert.equal(await procErr('get_posthog_config_status', [], other), 'posthog.not_authorized');
  const [me] = await sql('SELECT * FROM posthog_admin_identity');
  assert.equal(await procErr('posthog_remove_admin_identity', [me.identity]), 'posthog.cannot_remove_last_admin');
});

await test('enqueue_event (admin): input validation rolls back; idempotency keys dedupe', async () => {
  const stranger = await newIdentity();
  assert.match(await callErr('posthog_enqueue_event', ['u1', 'ev', opt(), opt()], stranger), /posthog\.not_authorized/);
  for (const [args, code] of [
    [['  ', 'ev', opt(), opt()], 'posthog.invalid_distinct_id'],
    [['d'.repeat(257), 'ev', opt(), opt()], 'posthog.invalid_distinct_id'],
    [['u1', '', opt(), opt()], 'posthog.invalid_event'],
    [['u1', 'e'.repeat(201), opt(), opt()], 'posthog.invalid_event'],
    [['u1', 'ev', opt('[1,2]'), opt()], 'posthog.invalid_properties_json'],
    [['u1', 'ev', opt('null'), opt()], 'posthog.invalid_properties_json'],
    [['u1', 'ev', opt(`{"a":"${'x'.repeat(65536)}"}`), opt()], 'posthog.properties_too_large'],
    [['u1', 'ev', opt(), opt('k'.repeat(257))], 'posthog.idempotency_key_too_long'],
  ]) {
    assert.match(await callErr('posthog_enqueue_event', args), new RegExp(code.replace('.', '\\.')), code);
  }
  assert.equal((await outbox()).length, 0, 'the reducer failed, so nothing was stored');
  await call('posthog_enqueue_event', ['  u1  ', ' signed_up ', opt('{"plan":"pro"}'), opt('  signup:u1 ')]);
  await call('posthog_enqueue_event', ['u1', 'signed_up', opt('{"plan":"other"}'), opt('signup:u1')]);
  const rows = await outbox();
  assert.equal(rows.length, 1);
  assert.equal(rows[0].outbox_id, 'idem:signup:u1');
  assert.equal(rows[0].distinct_id, 'u1');
  assert.equal(rows[0].event, 'signed_up');
  assert.equal(unopt(rows[0].properties_json), '{"plan":"pro"}');
  assert.equal(rows[0].status, 'Queued');
  assert.match(rows[0].uuid, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.deepEqual(await stats(), [1, 0, 0]);
});

await test('host reducer: anyone; invalid analytics input is stored Rejected, the order still commits', async () => {
  const shopper = await newIdentity();
  await call('complete_order', ['o1', 1999, opt()], shopper);
  await call('complete_order', ['o2', 500, opt('not json')], shopper);
  const ok = await outboxRow('idem:order_completed:o1');
  assert.equal(ok.status, 'Queued');
  assert.equal(ok.distinct_id, shopper.identity);
  const rejected = (await outbox()).find((r) => r.status === 'Rejected');
  assert.match(rejected.outbox_id, /^evt:/, 'a rejected event gets a uuid id, not the idempotency key');
  assert.equal(unopt(rejected.last_error), 'posthog.invalid_properties_json');
  assert.equal(unopt(rejected.properties_json), undefined);
  assert.equal(unopt(rejected.idempotency_key), undefined);
  assert.equal((await sql('SELECT * FROM demo_order')).length, 2);
  assert.deepEqual(await stats(), [2, 0, 1]);
});

await test('flush_outbox: batch body (api_key, uuid, ISO timestamp, properties in JS order); delivered', async () => {
  assert.equal(await procErr('posthog_flush_outbox', [0]), 'posthog.invalid_flush_limit');
  assert.equal(await procErr('posthog_flush_outbox', [101]), 'posthog.invalid_flush_limit');
  await call('posthog_enqueue_event', ['u2', 'keys', opt('{"b":1,"10":2.0,"a":"é🎉"}'), opt()]);
  mock.clear();
  const res = unwrap(await call('posthog_flush_outbox', [100]));
  assert.deepEqual(res, [3, 3, 0]);
  const [body] = batches();
  assert.equal(body.api_key, 'phc_test_key');
  assert.equal(body.batch.length, 3);
  const ev = body.batch.find((e) => e.event === 'keys');
  const raw = mock.requests[0].body;
  assert.ok(raw.includes('"properties":{"10":2,"b":1,"a":"é🎉"}'), raw);
  assert.match(ev.timestamp, /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{6}Z$/);
  const row = (await outbox()).find((r) => r.event === 'keys');
  assert.equal(ev.uuid, row.uuid);
  const micros = BigInt(row.created_at[0]);
  assert.equal(ev.timestamp, new Date(Number(micros / 1000n)).toISOString().replace(/\.\d{3}Z$/, `.${String(micros % 1000000n).padStart(6, '0')}Z`));
  assert.equal(row.status, 'Delivered');
  assert.equal(unopt(row.last_status_code), 200);
  assert.ok(unopt(row.delivered_at));
  assert.deepEqual(await stats(), [0, 3, 1]);
  const log = await sql('SELECT * FROM posthog_delivery_log');
  assert.equal(log.filter((l) => SOURCE[l.source[0]] === 'Flush' && l.ok).length, 3);
  assert.deepEqual(unwrap(await call('posthog_flush_outbox', [10])), [0, 0, 0], 'nothing left');
});

await test('retries: 5xx and network errors back off (1 s, 2 s …), 4xx fails at once, requeue resets', async () => {
  await call('posthog_enqueue_event', ['u3', 'retry_me', opt(), opt('retry:1')]);
  batchMode = '503';
  assert.deepEqual(unwrap(await call('posthog_flush_outbox', [10])), [1, 0, 1]);
  let r = await outboxRow('idem:retry:1');
  assert.equal(r.status, 'Queued');
  assert.equal(r.attempts, 1);
  assert.equal(unopt(r.last_status_code), 503);
  assert.equal(unopt(r.last_error), 'mock 503');
  assert.equal(BigInt(r.next_attempt_at[0]) - BigInt(r.updated_at[0]), 1_000_000n, 'first retry after 1 s');
  assert.deepEqual(unwrap(await call('posthog_flush_outbox', [10])), [0, 0, 0], 'not due yet');
  await sleep(1100);
  batchMode = 'drop';
  assert.deepEqual(unwrap(await call('posthog_flush_outbox', [10])), [1, 0, 1]);
  r = await outboxRow('idem:retry:1');
  assert.equal(r.attempts, 2);
  assert.equal(unopt(r.last_status_code), 0, 'network errors are status 0 and retry');
  assert.equal(BigInt(r.next_attempt_at[0]) - BigInt(r.updated_at[0]), 2_000_000n);
  await sleep(2100);
  batchMode = '401';
  assert.deepEqual(unwrap(await call('posthog_flush_outbox', [10])), [1, 0, 1]);
  r = await outboxRow('idem:retry:1');
  assert.equal(r.status, 'Failed', 'auth errors fail at once');
  assert.equal(r.attempts, 3);
  assert.match(await callErr('posthog_requeue_failed_events', [0]), /posthog\.invalid_requeue_limit/);
  await call('posthog_requeue_failed_events', [10]);
  r = await outboxRow('idem:retry:1');
  assert.deepEqual([r.status, r.attempts], ['Queued', 0]);
  batchMode = 'ok';
  assert.deepEqual(unwrap(await call('posthog_flush_outbox', [10])), [1, 1, 0]);
  assert.equal((await outboxRow('idem:retry:1')).status, 'Delivered');
});

await test('408/429 retry; the fifth failure is final; long bodies are truncated in logs', async () => {
  await call('posthog_enqueue_event', ['u4', 'exhaust', opt(), opt('exhaust:1')]);
  const waits = [0, 1100, 2100, 4100, 8100];
  for (let i = 0; i < 5; i++) {
    await sleep(waits[i]);
    batchMode = i === 0 ? '408' : i === 1 ? '429' : i === 4 ? 'big' : '503';
    assert.deepEqual(unwrap(await call('posthog_flush_outbox', [10])), [1, 0, 1], `attempt ${i + 1}`);
  }
  const r = await outboxRow('idem:exhaust:1');
  assert.equal(r.status, 'Failed');
  assert.equal(r.attempts, 5);
  assert.equal(unopt(r.last_error), 'y'.repeat(2048) + '...');
  batchMode = 'ok';
});

await test('capture_now: one direct event, logged as Direct; errors reported', async () => {
  assert.equal(await procErr('posthog_capture_now', ['u5', '', opt()]), 'posthog.invalid_event');
  mock.clear();
  assert.deepEqual(unwrap(await call('posthog_capture_now', ['  raw-id ', 'clicked', opt('{"x":1}')])), [true, 200, [1, []]]);
  const [body] = batches();
  assert.equal(body.batch[0].distinct_id, '  raw-id ', 'capture_now sends the id as given');
  assert.deepEqual(body.batch[0].properties, { x: 1 });
  batchMode = '503';
  assert.deepEqual(unwrap(await call('posthog_capture_now', ['u5', 'clicked', opt()])), [false, 503, [0, 'mock 503']]);
  batchMode = 'ok';
  const direct = (await sql('SELECT * FROM posthog_delivery_log')).filter((l) => SOURCE[l.source[0]] === 'Direct');
  assert.equal(direct.length, 2);
  assert.equal(unopt(direct.find((d) => !d.ok).error_message), 'mock 503');
});

await test('get_feature_flag: enabled, disabled, variant, missing, invalid; request shape; errors', async () => {
  for (const [args, code] of [
    [['  ', 'u1', opt(), opt()], 'posthog.invalid_flag_key'],
    [['k'.repeat(201), 'u1', opt(), opt()], 'posthog.invalid_flag_key'],
    [['enabled', ' ', opt(), opt()], 'posthog.invalid_distinct_id'],
    [['enabled', 'u1', opt('[]'), opt()], 'posthog.invalid_person_properties_json'],
    [['enabled', 'u1', opt(), opt('"x"')], 'posthog.invalid_groups_json'],
    [['enabled', 'u1', opt(`{"a":"${'x'.repeat(65536)}"}`), opt()], 'posthog.person_properties_too_large'],
  ]) {
    assert.equal(await procErr('posthog_get_feature_flag', args), code);
  }
  mock.clear();
  assert.deepEqual(unwrap(await call('posthog_get_feature_flag', ['enabled', 'u1', opt('{"plan":"pro"}'), opt('{"company":"acme"}')])), [true, 200, [0, true], [1, []], [1, []]]);
  assert.equal(mock.last().body, '{"api_key":"phc_test_key","distinct_id":"u1","person_properties":{"plan":"pro"},"groups":{"company":"acme"}}');
  assert.deepEqual(unwrap(await call('posthog_get_feature_flag', ['disabled', 'u1', opt(), opt()])), [true, 200, [0, false], [1, []], [1, []]]);
  assert.equal(mock.last().body, '{"api_key":"phc_test_key","distinct_id":"u1"}', 'undefined keys are omitted');
  assert.deepEqual(unwrap(await call('posthog_get_feature_flag', ['experiment', 'u1', opt(), opt()])), [true, 200, [0, true], [0, 'control'], [1, []]]);
  for (const key of ['invalid', 'missing', 'toString']) {
    assert.deepEqual(unwrap(await call('posthog_get_feature_flag', [key, 'u1', opt(), opt()])), [true, 200, [1, []], [1, []], [1, []]], key);
  }
  assert.deepEqual(unwrap(await call('posthog_get_feature_flag', ['enabled', 'down', opt(), opt()])), [false, 500, [1, []], [1, []], [0, 'flags exploded']]);
  const logged = (await sql('SELECT * FROM posthog_delivery_log')).filter((l) => SOURCE[l.source[0]] === 'FeatureFlag');
  assert.ok(logged.some((l) => l.event === '$feature_flag:experiment'));
  assert.ok(logged.every((l) => l.response_body.length <= 2051), 'long bodies truncated in the log');
});

await test('admin views: outbox (in-flight, queued) and the 50 newest log rows; empty for others', async () => {
  await call('posthog_enqueue_event', ['u6', 'queued_for_view', opt(), opt()]);
  const view = await sql('SELECT * FROM posthog_outbox_admin');
  assert.ok(view.some((r) => r.event === 'queued_for_view'));
  assert.ok(view.every((r) => ['Queued', 'Processing'].includes(STATUS[r.status[0]])));
  const log = await sql('SELECT * FROM posthog_delivery_log_admin');
  assert.ok(log.length > 0 && log.length <= 50);
  assert.equal(log[0].attempted_at_order, undefined, 'the view row omits attempted_at_order');
  const stranger = await newIdentity();
  assert.equal((await sql('SELECT * FROM posthog_outbox_admin', stranger)).length, 0);
  assert.equal((await sql('SELECT * FROM posthog_delivery_log_admin', stranger)).length, 0);
});

await test('retention: delivered/failed/rejected rows and log rows older than 30 days are pruned', async () => {
  const before = await outbox();
  const finished = before.filter((r) => ['Delivered', 'Failed', 'Rejected'].includes(r.status)).length;
  assert.ok(finished > 0);
  await call('demo_prune_history_at', [(Date.now() + 29 * 86400_000) * 1000]);
  assert.equal((await outbox()).length, before.length);
  await call('demo_prune_history_at', [(Date.now() + 31 * 86400_000) * 1000]);
  const after = await outbox();
  assert.equal(after.length, before.length - finished);
  assert.ok(after.every((r) => ['Queued', 'Processing'].includes(r.status)));
  assert.equal((await sql('SELECT * FROM posthog_delivery_log')).length, 0);
  const [pending, delivered, failed] = await stats();
  assert.deepEqual([delivered, failed], [0, 0]);
  assert.equal(pending, after.length);
});

await test('clear_analytics: bounded deletes', async () => {
  assert.match(await callErr('posthog_clear_analytics', [0]), /posthog\.invalid_clear_batch/);
  assert.match(await callErr('posthog_clear_analytics', [10001]), /posthog\.invalid_clear_batch/);
  await call('posthog_clear_analytics', [10000]);
  assert.equal((await outbox()).length, 0);
  assert.deepEqual(await stats(), [0, 0, 0]);
});

await test('the scheduled flush delivers queued events on its own (every 5 s); clients can not call it', async () => {
  await call('demo_resume_scheduler', []);
  await call('posthog_enqueue_event', ['u7', 'scheduled', opt(), opt('sched:1')]);
  await until(async () => (await outboxRow('idem:sched:1'))?.status === 'Delivered', 12_000, 'scheduled delivery');
  assert.ok(await callErr('posthog_scheduled_flush', [{ scheduled_id: 1, scheduled_at: { Interval: { __time_duration_micros__: 1 } } }]));
  await call('demo_pause_scheduler', []);
});

await mock.close();
done();
