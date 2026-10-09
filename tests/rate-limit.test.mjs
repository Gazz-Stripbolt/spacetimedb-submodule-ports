// @spacetimedb/rate-limit port: the upstream test cases (scripts/test.ts at the pinned
// commit) against a real module, plus the admin reducers, sweep and view.
import assert from 'node:assert/strict';
import { call, callErr, procErr, sql, newIdentity, test, done, unwrap, sleep, owner, some, none } from './lib.mjs';

const run = Date.now().toString(36);
const S = (name) => `${name}.${run}`; // scopes are unique per run
const consume = async (scope, limit, window, key, cost = 1) => {
  const [allowed, k, sc, lim, used, remaining, resetAt, retryAfter] = unwrap(await call('demo_consume', [scope, limit, window, key, cost]));
  return { allowed, key: k, scope: sc, limit: lim, used, remaining, resetAt: BigInt(resetAt[0]), retryAfter };
};
const consumeErr = async (scope, limit, window, key, cost = 1) => {
  const r = await call('demo_consume', [scope, limit, window, key, cost]);
  return r[0] === 1 ? r[1] : null;
};
const peek = async (scope, limit, window, key, useNow) => {
  const [lim, win, used, remaining, resetAt] = unwrap(await call('demo_peek', [scope, limit, window, key, useNow]));
  return { limit: lim, window: win, used, remaining, resetAt: resetAt[0] === 0 ? BigInt(resetAt[1][0]) : null };
};

await test('limits per scope and per actor', async () => {
  assert.equal((await consume(S('tap'), 1, 60, 'alice')).allowed, true, 'first call allowed');
  assert.equal((await consume(S('tap'), 1, 60, 'alice')).allowed, false, 'limit enforced');
  assert.equal((await consume(S('other'), 1, 60, 'alice')).allowed, true, 'scopes are independent');
  assert.equal((await consume(S('tap'), 1, 60, 'bob')).allowed, true, 'actors are independent');
});

await test('a scope keeps one policy; invalid policies and actor keys are rejected', async () => {
  assert.equal(await consumeErr(S('tap'), 5, 30, 'alice'), 'rate_limit.duplicate_scope');
  assert.equal(await consumeErr('', 1, 1, 'a'), 'rate_limit.invalid_scope');
  assert.equal(await consumeErr('x'.repeat(129), 1, 1, 'a'), 'rate_limit.invalid_scope');
  assert.equal(await consumeErr(S('nolimit'), 0, 1, 'a'), 'rate_limit.invalid_limit');
  assert.equal(await consumeErr(S('nowindow'), 1, 0, 'a'), 'rate_limit.invalid_window');
  assert.equal(await consumeErr(S('tap'), 1, 60, ''), 'rate_limit.invalid_actor_key');
  assert.equal(await consumeErr(S('tap'), 1, 60, 'k'.repeat(257)), 'rate_limit.invalid_actor_key');
});

await test('cost: above the limit or zero is invalid and leaves the bucket alone; exactly the limit fills it', async () => {
  assert.equal(await consumeErr(S('upload'), 3, 60, 'alice', 4), 'rate_limit.invalid_cost');
  assert.equal(await consumeErr(S('upload'), 3, 60, 'alice', 0), 'rate_limit.invalid_cost');
  assert.equal((await sql(`SELECT * FROM rate_limit_bucket WHERE scope = '${S('upload')}'`)).length, 0);
  const full = await consume(S('upload'), 3, 60, 'alice', 3);
  assert.ok(full.allowed && full.used === 3 && full.remaining === 0);
});

await test('counts, retry-after, peek, and a fresh window after expiry', async () => {
  const login = S('auth.login');
  const one = await consume(login, 2, 2, 'ip:1');
  const two = await consume(login, 2, 2, 'ip:1');
  const three = await consume(login, 2, 2, 'ip:1');
  assert.ok(one.allowed && one.remaining === 1);
  assert.ok(two.allowed && two.remaining === 0);
  assert.ok(!three.allowed && three.used === 2 && three.remaining === 0 && three.retryAfter >= 1 && three.retryAfter <= 2);
  assert.equal(three.resetAt, one.resetAt, 'the window is anchored at its first request');
  const status = await peek(login, 2, 2, 'ip:1', true);
  assert.ok(status.used === 2 && status.remaining === 0 && status.resetAt === one.resetAt, 'peek reports the open window');
  assert.equal((await peek(login, 2, 2, 'ip:2', false)).used, 0, 'an unused bucket is fresh');
  await sleep(2100);
  const expired = await peek(login, 2, 2, 'ip:1', true);
  assert.ok(expired.used === 0 && expired.resetAt === null, 'peek with a clock reports an expired window as fresh');
  assert.equal((await peek(login, 2, 2, 'ip:1', false)).used, 2, 'peek without a clock still shows the stale row');
  const next = await consume(login, 2, 2, 'ip:1');
  assert.ok(next.allowed && next.used === 1, 'an expired window resets');
});

await test('bucket keys length-prefix both parts, so delimiters cannot collide', async () => {
  const a = await consume(S('a:actor:b'), 1, 1, 'c');
  const b = await consume(`${run}.a`, 1, 1, `b:actor:c`);
  assert.notEqual(a.key, b.key);
  assert.equal(a.key, `${S('a:actor:b').length}:${S('a:actor:b')}1:c`);
  // UTF-16 lengths, like TS: 'é' is 1 unit, '😀' is 2.
  assert.equal((await consume(S('u'), 1, 60, 'é😀')).key, `${S('u').length}:${S('u')}3:é😀`);
});

await test('sweep removes expired buckets only; admins only; max_rows validated', async () => {
  await consume(S('sweep.long'), 1, 100, 'a');
  await consume(S('sweep.long'), 1, 100, 'b');
  const short = (await consume(S('sweep.short'), 1, 1, 'a')).key;
  await sleep(1100);
  const stranger = await newIdentity();
  assert.equal(await procErr('rate_limit_run_sweep', [none], stranger), 'rate_limit.not_authorized');
  assert.equal(await procErr('rate_limit_run_sweep', [some(0)]), 'rate_limit.invalid_max_rows');
  assert.equal(await procErr('rate_limit_run_sweep', [some(10_001)]), 'rate_limit.invalid_max_rows');
  const deleted = unwrap(await call('rate_limit_run_sweep', [none]));
  assert.ok(deleted >= 1);
  const keys = (await sql('SELECT * FROM rate_limit_bucket')).map((r) => r.key);
  assert.ok(!keys.includes(short), 'expired bucket swept');
  assert.equal(keys.filter((k) => k.includes(S('sweep.long'))).length, 2, 'open buckets kept');
});

await test('install: the publisher is admin, config row exists, sweep tick armed', async () => {
  const cfg = await sql('SELECT * FROM rate_limit_config');
  assert.equal(cfg.length, 1);
  assert.equal(cfg[0].sweep_batch, 500);
  assert.equal((await sql('SELECT * FROM rate_limit_sweep_tick')).length, 1);
  assert.equal((await sql('SELECT * FROM rate_limit_admin_identity')).length, 1);
});

await test('admin reducers: update config, reset buckets, admin view, add/remove admins', async () => {
  const stranger = await newIdentity();
  const sid = { __identity__: '0x' + stranger.identity };
  assert.equal(await callErr('rate_limit_update_config', [100], stranger), 'rate_limit.not_authorized');
  assert.equal(await callErr('rate_limit_update_config', [0]), 'rate_limit.invalid_sweep_batch');
  await call('rate_limit_update_config', [100]);
  assert.equal((await sql('SELECT * FROM rate_limit_config'))[0].sweep_batch, 100);
  assert.equal((await sql('SELECT * FROM admin_rate_limit_buckets', stranger)).length, 0, 'strangers see nothing');
  assert.ok((await sql('SELECT * FROM admin_rate_limit_buckets')).length > 0, 'admins see buckets');
  assert.equal(await callErr('rate_limit_reset_buckets', [some(0)]), 'rate_limit.invalid_max_rows');
  await call('rate_limit_reset_buckets', [some(10_000)]);
  assert.equal((await sql('SELECT * FROM rate_limit_bucket')).length, 0);
  await call('add_rate_limit_admin', [sid]);
  await call('rate_limit_update_config', [200], stranger);
  await call('remove_rate_limit_admin', [sid]);
  assert.equal(await callErr('rate_limit_update_config', [300], stranger), 'rate_limit.not_authorized');
  const me = (await sql('SELECT * FROM rate_limit_admin_identity'))[0].identity;
  assert.equal(await callErr('remove_rate_limit_admin', [me]), 'rate_limit.cannot_remove_last_admin');
});

done();
