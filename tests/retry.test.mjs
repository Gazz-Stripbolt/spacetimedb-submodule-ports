// @spacetimedb/retry port: the upstream test cases (scripts/test.ts at the pinned commit),
// replayed against a real module and the real scheduler instead of a mocked context.
import assert from 'node:assert/strict';
import { call, callErr, sql, newIdentity, test, done, until, owner, sleep } from './lib.mjs';

const tag = (name, value = []) => ({ [name]: value });
const submit = (name, args, maxAttempts = 3, backoffSecs = 1, who = owner) =>
  call('submit_retry_task', [name, args, maxAttempts, backoffSecs], who);
const history = (name) => sql(`SELECT * FROM retry_history WHERE task_name = '${name}'`);
const status = (row) => ['Ok', 'Failed', 'GaveUp'][row.status[0]];
const err = (row) => (row.error[0] === 0 ? row.error[1] : null);
const micros = (ts) => BigInt(ts[0]);
const run = `r${Date.now().toString(36)}`; // unique task names per run

await test('submission validates input', async () => {
  const valid = [`${run}-job`, tag('succeeds'), 3, 2];
  assert.equal(await callErr('submit_retry_task', [''].concat(valid.slice(1))), 'retry.invalid_task_name');
  assert.equal(await callErr('submit_retry_task', ['x'.repeat(129)].concat(valid.slice(1))), 'retry.invalid_task_name');
  assert.equal(await callErr('submit_retry_task', [valid[0], valid[1], 11, 2]), 'retry.invalid_max_attempts');
  assert.equal(await callErr('submit_retry_task', [valid[0], valid[1], 0, 2]), 'retry.invalid_max_attempts');
  assert.equal(await callErr('submit_retry_task', [valid[0], valid[1], 3, 0]), 'retry.invalid_backoff_seconds');
  assert.equal(await callErr('submit_retry_task', [valid[0], valid[1], 3, 3601]), 'retry.invalid_backoff_seconds');
  // A pending task blocks a second one with the same name. Back it off so it stays pending.
  await submit(`${run}-dup`, tag('fails'), 2, 3600);
  await until(async () => (await history(`${run}-dup`)).length === 1, 5000, 'first attempt');
  assert.equal(await callErr('submit_retry_task', [`${run}-dup`, tag('succeeds'), 3, 2]), `retry.task_already_exists:${run}-dup`);
});

await test('only retry admins can submit or see tasks and history; the scheduled reducer is not client-callable', async () => {
  const stranger = await newIdentity();
  assert.equal(await callErr('submit_retry_task', [`${run}-other`, tag('succeeds'), 1, 1], stranger), 'retry.not_authorized');
  assert.equal((await sql('SELECT * FROM retry_tasks_admin', stranger)).length, 0);
  assert.equal((await sql('SELECT * FROM retry_history_admin', stranger)).length, 0);
  assert.ok((await sql('SELECT * FROM retry_history_admin')).length > 0, 'owner sees history');
  const fake = [0, { Time: [0] }, `${run}-fake`, tag('succeeds'), 0, 1, 1];
  // 2.11 doesn't expose scheduled reducers over HTTP ("No such procedure"); retry_fire also
  // checks the caller itself, in case a transport does.
  assert.match(await callErr('retry_fire', [fake], stranger), /No such procedure|retry\.not_authorized/);
});

await test('success on the first attempt: one Ok row, nothing rescheduled', async () => {
  await submit(`${run}-ok`, tag('flaky', [`${run}-ok`, 0, false]));
  const rows = await until(async () => {
    const r = await history(`${run}-ok`);
    return r.length ? r : null;
  });
  assert.deepEqual(rows.map(status), ['Ok']);
  const metric = await sql(`SELECT * FROM retry_metric WHERE name = 'flaky-success-${run}-ok'`);
  assert.equal(metric.length, 1);
  assert.equal((await sql(`SELECT * FROM retry_task WHERE name = '${run}-ok'`)).length, 0);
});

await test('failures back off exponentially (backoff × 2^attempt) until an attempt succeeds', async () => {
  await submit(`${run}-flaky`, tag('flaky', [`${run}-flaky`, 2, true]), 3, 1);
  const rows = await until(async () => {
    const r = (await history(`${run}-flaky`)).sort((a, b) => a.attempt - b.attempt);
    return r.length === 3 ? r : null;
  }, 10_000, 'three attempts');
  assert.deepEqual(rows.map(status), ['Failed', 'Failed', 'Ok']);
  assert.equal(err(rows[0]), 'simulated failure at attempt 0');
  const gap1 = Number(micros(rows[1].ran_at) - micros(rows[0].ran_at)) / 1e6;
  const gap2 = Number(micros(rows[2].ran_at) - micros(rows[1].ran_at)) / 1e6;
  assert.ok(gap1 >= 1 && gap1 < 1.8, `first backoff ~1 s, got ${gap1}`);
  assert.ok(gap2 >= 2 && gap2 < 2.8, `second backoff ~2 s, got ${gap2}`);
});

await test('gives up after maxAttempts; errors are truncated to 2,048 characters', async () => {
  await submit(`${run}-long`, tag('longError'), 2, 1);
  const rows = await until(async () => {
    const r = (await history(`${run}-long`)).sort((a, b) => a.attempt - b.attempt);
    return r.length === 2 ? r : null;
  }, 6000, 'two attempts');
  assert.deepEqual(rows.map(status), ['Failed', 'GaveUp']);
  assert.equal(err(rows[0]).length, 2048);
  await sleep(1500);
  assert.equal((await sql(`SELECT * FROM retry_task WHERE name = '${run}-long'`)).length, 0, 'nothing pending');
});

await test('a returned failure records its message', async () => {
  await submit(`${run}-fails`, tag('fails'), 1, 1);
  const [row] = await until(async () => {
    const r = await history(`${run}-fails`);
    return r.length ? r : null;
  });
  assert.equal(status(row), 'GaveUp');
  assert.equal(err(row), 'unavailable');
});

await test('a permanent error gives up on the first attempt without scheduling another', async () => {
  await submit(`${run}-rejects`, tag('rejects'), 5, 1);
  const [row] = await until(async () => {
    const r = await history(`${run}-rejects`);
    return r.length ? r : null;
  });
  assert.equal(status(row), 'GaveUp');
  assert.equal(err(row), 'order.not_found');
  await sleep(1500);
  assert.equal((await history(`${run}-rejects`)).length, 1);
});

await test('the admin task view lists every pending task', async () => {
  await submit(`${run}-p1`, tag('fails'), 2, 3600);
  await submit(`${run}-p2`, tag('fails'), 2, 3600);
  await until(async () => (await history(`${run}-p2`)).length === 1, 5000, 'p2 attempted');
  // (SQL over a view doesn't keep the view's row order, so only the contents are checked.)
  const view = (await sql('SELECT * FROM retry_tasks_admin')).map((r) => r.scheduled_id).sort();
  const table = (await sql('SELECT * FROM retry_task')).map((r) => r.scheduled_id).sort();
  assert.deepEqual(view, table);
  assert.ok(view.length >= 2);
});

await test('history keeps the latest 1,000 attempts, dropping the oldest first', async () => {
  await call('demo_run_inline', [1100]);
  const rows = await sql('SELECT * FROM retry_history_admin');
  assert.equal(rows.length, 1000);
  assert.equal((await sql('SELECT * FROM retry_history')).length, 1000);
  // The older attempts from the tests above went first; only inline attempts remain.
  // (All 1,100 share one timestamp, so which of those survive isn't specified.)
  assert.ok(rows.every((r) => r.task_name === 'inline' && status(r) === 'Ok'));
});

await test('admins: add, remove, and the last admin cannot be removed', async () => {
  const friend = await newIdentity();
  assert.equal(await callErr('add_retry_admin_identity', [{ __identity__: '0x' + friend.identity }], friend), 'retry.not_authorized');
  await call('add_retry_admin_identity', [{ __identity__: '0x' + friend.identity }]);
  await submit(`${run}-friend`, tag('succeeds'), 1, 1, friend);
  await call('remove_retry_admin_identity', [{ __identity__: '0x' + friend.identity }]);
  assert.equal(await callErr('submit_retry_task', [`${run}-friend2`, tag('succeeds'), 1, 1], friend), 'retry.not_authorized');
  const me = (await sql('SELECT * FROM retry_admin_identity'))[0].identity;
  assert.equal(await callErr('remove_retry_admin_identity', [me]), 'retry.cannot_remove_last_admin');
});

done();
