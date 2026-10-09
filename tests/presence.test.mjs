// @spacetimedb/presence port: the upstream test cases (scripts/test.ts at the pinned commit)
// against a real module, plus heartbeat, the views, config and the admin reducers.
import assert from 'node:assert/strict';
import { call, callErr, procErr, sql, newIdentity, test, done, unwrap, sleep, some, none } from './lib.mjs';

const run = Date.now().toString(36);
const S = (name) => `${name}.${run}`; // scopes are unique per run
const opt = (v) => (v === undefined ? none : some(v));
const fromOpt = (o) => (o[0] === 0 ? o[1] : undefined);
const micros = (ts) => BigInt(ts[0]);
const entry = (r) => {
  const [key, scope, subject, status, activity, payloadJson, joinedAt, lastSeenAt, expiresAt, updatedAt] = r;
  return {
    key, scope, subject, status,
    activity: fromOpt(activity), payloadJson: fromOpt(payloadJson),
    joinedAt: micros(joinedAt), lastSeenAt: micros(lastSeenAt), expiresAt: micros(expiresAt), updatedAt: micros(updatedAt),
  };
};
const upsert = async (o) =>
  entry(unwrap(await call('demo_upsert', [o.scope, o.subject, opt(o.status), opt(o.activity), opt(o.payloadJson), opt(o.ttlSeconds)])));
const upsertErr = (o) =>
  procErr('demo_upsert', [o.scope, o.subject, opt(o.status), opt(o.activity), opt(o.payloadJson), opt(o.ttlSeconds)]);
const touch = async (scope, subject, ttl) => entry(unwrap(await call('demo_touch', [scope, subject, opt(ttl)])));
const remove = async (scope, subject) => unwrap(await call('demo_remove', [scope, subject]));
const key = async (scope, subject) => unwrap(await call('demo_key', [scope, subject]));
const SEC = 1_000_000n;
const rowsIn = async (scope) => sql(`SELECT * FROM presence_entry WHERE scope = '${scope}'`);

await test('install: the publisher is admin, config is seeded (30 s TTL, 500 batch), sweep armed every 10 s', async () => {
  const cfg = await sql('SELECT * FROM presence_config');
  assert.equal(cfg.length, 1);
  assert.equal(cfg[0].default_ttl_seconds, 30);
  assert.equal(cfg[0].sweep_batch, 500);
  assert.equal((await sql('SELECT * FROM presence_admin_identity')).length, 1);
  assert.equal((await sql('SELECT * FROM presence_sweep_tick')).length, 1);
});

await test('upsert and touch use the configured default TTL', async () => {
  await call('presence_update_config', [45, 500]);
  try {
    const row = await upsert({ scope: S('room:1'), subject: 'user:alice' });
    assert.equal(row.expiresAt - row.joinedAt, 45n * SEC);
    const touched = await touch(S('room:1'), 'user:alice');
    assert.equal(touched.expiresAt - touched.lastSeenAt, 45n * SEC);
    assert.equal(touched.joinedAt, row.joinedAt, 'touch keeps joined_at');
  } finally {
    await call('presence_update_config', [30, 500]);
  }
});

await test('upsert validates scope, subject, status, activity, payload and TTL', async () => {
  const base = { scope: S('room:v'), subject: 'user:alice' };
  assert.equal(await upsertErr({ ...base, scope: ' ' }), 'presence.invalid_scope');
  assert.equal(await upsertErr({ ...base, scope: 's'.repeat(129) }), 'presence.invalid_scope');
  assert.equal(await upsertErr({ ...base, subject: '' }), 'presence.invalid_subject');
  assert.equal(await upsertErr({ ...base, subject: 'u'.repeat(257) }), 'presence.invalid_subject');
  assert.equal(await upsertErr({ ...base, status: '  ' }), 'presence.invalid_status');
  assert.equal(await upsertErr({ ...base, status: 'x'.repeat(65) }), 'presence.invalid_status');
  assert.equal(await upsertErr({ ...base, activity: 'a'.repeat(257) }), 'presence.invalid_activity');
  assert.equal(await upsertErr({ ...base, payloadJson: 'x'.repeat(4097) }), 'presence.invalid_payload');
  assert.equal(await upsertErr({ ...base, ttlSeconds: 0 }), 'presence.invalid_ttl_seconds');
  assert.equal(await upsertErr({ ...base, ttlSeconds: 3601 }), 'presence.invalid_ttl_seconds');
  assert.equal((await rowsIn(S('room:v'))).length, 0, 'nothing written');
  // At the limits, and blank optional strings become none.
  const ok = await upsert({ ...base, status: 'x'.repeat(64), activity: '   ', payloadJson: ' '.repeat(5000), ttlSeconds: 3600 });
  assert.equal(ok.activity, undefined);
  assert.equal(ok.payloadJson, undefined);
  assert.equal(ok.status.length, 64);
});

await test('keys are trimmed and length-prefixed, so delimiters cannot collide', async () => {
  assert.notEqual(await key('room::one', 'user'), await key('room', 'one::user'));
  assert.equal(await key('  room:1 ', ' alice '), '6:room:15:alice');
  // UTF-16 lengths, like TS: 'é' is 1 unit, '😀' is 2.
  assert.equal(await key('é😀', 'x'), '3:é😀1:x');
});

await test('touch preserves status, activity and payload; touch creates a missing entry', async () => {
  await upsert({ scope: S('room:t'), subject: 'user:alice', status: 'away', activity: 'editing', payloadJson: '{"cursor":4}', ttlSeconds: 30 });
  const row = await touch(S('room:t'), 'user:alice', 30);
  assert.equal(row.status, 'away');
  assert.equal(row.activity, 'editing');
  assert.equal(row.payloadJson, '{"cursor":4}');
  const created = await touch(S('room:t'), 'user:bob', 10);
  assert.equal(created.status, 'online');
  assert.equal(created.expiresAt - created.joinedAt, 10n * SEC);
});

await test('upsert updates status and keeps one row', async () => {
  const first = await upsert({ scope: S('room:u'), subject: 'user:alice', status: 'online', ttlSeconds: 30 });
  await sleep(5);
  const row = await upsert({ scope: S('room:u'), subject: 'user:alice', status: 'away', ttlSeconds: 30 });
  assert.equal(row.status, 'away');
  assert.equal(row.joinedAt, first.joinedAt, 'joined_at kept');
  assert.equal((await rowsIn(S('room:u'))).length, 1);
});

await test('remove returns true for an existing entry, then false', async () => {
  await upsert({ scope: S('room:r'), subject: 'user:alice', ttlSeconds: 10 });
  assert.equal(await remove(S('room:r'), 'user:alice'), true);
  assert.equal((await rowsIn(S('room:r'))).length, 0);
  assert.equal(await remove(S('room:r'), 'user:alice'), false);
});

await test('config: admins only, validated, config_missing never hit after install', async () => {
  const stranger = await newIdentity();
  assert.equal(await callErr('presence_update_config', [45, 750], stranger), 'presence.not_authorized');
  assert.equal(await callErr('presence_update_config', [45, 10_001]), 'presence.invalid_sweep_batch');
  assert.equal(await callErr('presence_update_config', [45, 0]), 'presence.invalid_sweep_batch');
  assert.equal(await callErr('presence_update_config', [0, 500]), 'presence.invalid_ttl_seconds');
  await call('presence_update_config', [45, 750]);
  const cfg = (await sql('SELECT * FROM presence_config'))[0];
  assert.equal(cfg.default_ttl_seconds, 45);
  assert.equal(cfg.sweep_batch, 750);
  await call('presence_update_config', [30, 500]);
});

await test('sweep deletes expired entries only, up to the configured batch; admins only', async () => {
  const stranger = await newIdentity();
  assert.equal(await procErr('presence_run_sweep', [], stranger), 'presence.not_authorized');
  await call('presence_update_config', [30, 2]);
  try {
    const scope = S('room:sweep');
    for (const subject of ['fresh-a', 'expired-a', 'fresh-b', 'expired-b', 'expired-c']) {
      await upsert({ scope, subject, ttlSeconds: subject.startsWith('fresh') ? 100 : 1 });
    }
    await sleep(1100);
    const expiredNow = async () =>
      (await sql('SELECT * FROM presence_entry')).filter((r) => micros(r.expires_at) <= BigInt(Date.now()) * 1000n).length;
    // The scheduled sweep (every 10 s, same batch) may run in between; retry until a call is
    // observed on its own.
    for (let attempt = 0; ; attempt++) {
      const before = await expiredNow();
      const deleted = unwrap(await call('presence_run_sweep', []));
      const after = await expiredNow();
      if (before - after === deleted || attempt === 3) {
        assert.equal(deleted, Math.min(2, before), 'stops at the batch');
        break;
      }
    }
    while (unwrap(await call('presence_run_sweep', [])) > 0);
    const left = (await rowsIn(scope)).map((r) => r.subject).sort();
    assert.deepEqual(left, ['fresh-a', 'fresh-b'], 'unexpired rows kept');
  } finally {
    await call('presence_update_config', [30, 500]);
  }
});

await test('heartbeat: the caller\'s identity in presence.global, public view, clear', async () => {
  const alice = await newIdentity();
  const r = unwrap(await call('presence_heartbeat', [some('busy'), some(' coding '), none, some(20)], alice));
  const [scope, subject, status, expiresAt] = r;
  assert.equal(scope, 'presence.global');
  assert.equal(subject, alice.identity.toLowerCase().replace(/^0x/, ''));
  assert.equal(status, 'busy');
  assert.ok(micros(expiresAt) > 0n);
  const online = await sql('SELECT * FROM presence_online', alice);
  const mine = online.find((row) => row.subject === subject);
  assert.ok(mine, 'visible in presence_online');
  assert.equal(mine.status, 'busy');
  assert.equal(await procErr('presence_heartbeat', [none, none, none, some(0)], alice), 'presence.invalid_ttl_seconds');
  await call('clear_presence', [], alice);
  assert.ok(!(await sql('SELECT * FROM presence_online', alice)).some((row) => row.subject === subject), 'cleared');
});

await test('presence_entries_admin: admins see every scope, others nothing', async () => {
  await upsert({ scope: S('room:admin'), subject: 'user:alice', ttlSeconds: 60 });
  const stranger = await newIdentity();
  assert.equal((await sql('SELECT * FROM presence_entries_admin', stranger)).length, 0);
  assert.ok((await sql('SELECT * FROM presence_entries_admin')).some((r) => r.scope === S('room:admin')));
  assert.ok(!(await sql('SELECT * FROM presence_online', stranger)).some((r) => r.scope === S('room:admin')), 'online view is the global scope only');
});

await test('admin reducers: add/remove admins, last admin protected, scheduled sweep rejects clients', async () => {
  const stranger = await newIdentity();
  const sid = { __identity__: '0x' + stranger.identity };
  assert.equal(await callErr('add_presence_admin', [sid], stranger), 'presence.not_authorized');
  await call('add_presence_admin', [sid]);
  await call('presence_update_config', [30, 500], stranger);
  await call('remove_presence_admin', [sid]);
  assert.equal(await callErr('presence_update_config', [30, 500], stranger), 'presence.not_authorized');
  const me = (await sql('SELECT * FROM presence_admin_identity'))[0].identity;
  assert.equal(await callErr('remove_presence_admin', [me]), 'presence.cannot_remove_last_admin');
  const err = await callErr('presence_sweep', [[0, { Interval: [10_000_000] }]], stranger);
  assert.ok(err, 'clients cannot run the scheduled sweep');
});

done();
