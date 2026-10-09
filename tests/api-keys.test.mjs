// @spacetimedb/api-keys port: the upstream test cases (scripts/test.ts at the pinned commit),
// with key derivation, hashing and JSON normalization checked against Node's own HMAC,
// SHA-256 and JSON.parse/JSON.stringify, plus the procedures, reducers and views end to end.
import assert from 'node:assert/strict';
import { createHash, createHmac } from 'node:crypto';
import { call, callErr, procErr, sql, newIdentity, test, done, unwrap, sleep, some, none } from './lib.mjs';

const opt = (v) => (v === undefined ? none : some(v));
const fromOpt = (o) => (o[0] === 0 ? o[1] : undefined);
const micros = (ts) => BigInt(ts[0]);
const hex = (who) => who.identity.toLowerCase().replace(/^0x/, '');
const sha256 = (s) => createHash('sha256').update(s).digest('hex');
const derive = (secret, counter, us) =>
  createHmac('sha256', secret).update(`spacetimedb-api-keys secret ${counter} ${us}`).digest('hex');
const SECRET = 'test-operator-secret-0123456789abcdef';

const createResult = (r) => {
  const [keyId, key, prefix, ownerSubject, name, scopesJson, metadataJson, status, createdAt, expiresAt] = r;
  return { keyId, key, prefix, ownerSubject, name, scopesJson, metadataJson: fromOpt(metadataJson), status: status[0], createdAt: micros(createdAt), expiresAt: fromOpt(expiresAt) };
};
const verifyResult = (r) => {
  const [allowed, reason, keyId, prefix, ownerSubject, scopesJson, metadataJson] = r;
  return { allowed, reason, keyId: fromOpt(keyId), prefix: fromOpt(prefix), ownerSubject: fromOpt(ownerSubject), scopesJson: fromOpt(scopesJson), metadataJson: fromOpt(metadataJson) };
};
const create = async (who, o) =>
  createResult(unwrap(await call('create_api_key', [o.name ?? 'key', o.scopes ?? '["files:read"]', opt(o.metadata), opt(o.expires), opt(o.prefix)], who)));
const createErr = (who, o) =>
  procErr('create_api_key', [o.name ?? 'key', o.scopes ?? '["files:read"]', opt(o.metadata), opt(o.expires), opt(o.prefix)], who);
const verify = async (key, scope, action) => verifyResult(unwrap(await call('demo_verify', [key, opt(scope), opt(action)])));
const usageFor = async (keyId) => sql(`SELECT * FROM api_key_usage WHERE key_id = '${keyId}'`);

await test('keys: derivation, format, lookup prefix and hash match the TS algorithm (Node HMAC/SHA-256)', async () => {
  for (const [secret, counter, us, pfx] of [
    ['operator-secret', 1n, 5n, 'stdb_live'],
    ['operator-secret', 2n, 5n, 'stdb_live'],
    ['other-secret', 1n, 5n, 'stdb_test'],
    ['x'.repeat(64), 9007199254740991n, -1n, 'A1_'],
    ['é😀 secret', 42n, 1791580000123456n, 'stdb_live'],
  ]) {
    const [key, prefix, hash] = await call('demo_derive', [secret, Number(counter), Number(us), pfx]);
    const want = derive(secret, counter, us);
    assert.equal(key, `${pfx}_${want}`);
    assert.equal(prefix, `${pfx}_${want.slice(0, 16)}`);
    assert.equal(hash, sha256(key));
  }
});

await test('scopes: exact, prefix wildcard, global wildcard, blank requirement, bad JSON', async () => {
  const has = async (scopes, req) => call('demo_has_scope', [scopes, opt(req)]);
  assert.equal(await has('["files:*","jobs:read"]', 'files:write'), true);
  assert.equal(await has('["files:*","jobs:read"]', 'jobs:read'), true);
  assert.equal(await has('["files:*","jobs:read"]', 'admin:write'), false);
  assert.equal(await has('not-json', 'files:read'), false);
  assert.equal(await has('["*"]', 'anything:at:all'), true);
  assert.equal(await has('["files:read"]', undefined), true);
  assert.equal(await has('["files:read"]', '  '), true);
  assert.equal(await has('{"files:read":1}', 'files:read'), false);
});

await test('JSON normalization is byte-identical to JSON.stringify(JSON.parse(x))', async () => {
  const cases = [
    '{"b":1,"a":2}',
    '{"2":1,"1":2,"x":3,"10":4,"01":5,"4294967295":6,"4294967294":7}',
    '{"a":1.0,"b":1e2,"c":1e21,"d":1e-7,"e":0.000001,"f":-0,"g":123456789012345678901234567890,"h":0.1,"i":-1.5e-10,"j":1e400}',
    '{"s":"é😀\\u0001\\n\\"\\\\\\/\\u2028","t":"\\ud800x","u":"\\u00e9"}',
    '{"a":{"b":[1,2,{"c":null}]},"z":false,"a":true}',
    ' \n\t{ "spaced" : [ 1 , 2 ] } \r\n',
    '[]', '"s"', '12', 'null',
  ];
  for (const text of cases) {
    const got = await call('demo_json', [text]);
    assert.deepEqual(got, [0, JSON.stringify(JSON.parse(text))], text);
  }
  for (const bad of ['{', '[1,]', '01', '{"a":1}x', "'a'", '{a:1}', '"\t"', ' {}', '[1 2]', '-', '1.', '.5', '+1', 'nul', '{"a" 1}', '"\\x"']) {
    let threw = false;
    try { JSON.parse(bad); } catch { threw = true; }
    assert.ok(threw, `node rejects ${JSON.stringify(bad)}`);
    assert.equal((await call('demo_json', [bad]))[0], 1, `rejects ${JSON.stringify(bad)}`);
  }
});

const alice = await newIdentity();
const bob = await newIdentity();

await test('config: required before keys, admins only, secret and retention validated', async () => {
  assert.equal(await createErr(alice, {}), 'api_keys.config_missing');
  assert.equal(await callErr('set_api_keys_config', [some(SECRET), none], alice), 'api_keys.not_authorized');
  assert.equal(await callErr('set_api_keys_config', [none, none]), 'api_keys.secret_required');
  assert.equal(await callErr('set_api_keys_config', [some('short'), none]), 'api_keys.invalid_secret');
  assert.equal(await callErr('set_api_keys_config', [some(SECRET), some(3599)]), 'api_keys.invalid_usage_retention_seconds');
  assert.equal(await callErr('set_api_keys_config', [some(SECRET), some(315_360_001)]), 'api_keys.invalid_usage_retention_seconds');
  await call('set_api_keys_config', [some(SECRET), none]);
  let cfg = (await sql('SELECT * FROM api_key_config'))[0];
  assert.equal(cfg.usage_retention_seconds, 2_592_000);
  assert.equal(Number(cfg.counter), 0);
  await call('set_api_keys_config', [none, some(3600)]);
  cfg = (await sql('SELECT * FROM api_key_config'))[0];
  assert.equal(cfg.secret, SECRET, 'secret kept when omitted');
  assert.equal(cfg.usage_retention_seconds, 3600);
});

let aliceKey;
await test('create: key format and derivation, normalized fields, only the hash is stored', async () => {
  const counter = BigInt((await sql('SELECT * FROM api_key_config'))[0].counter) + 1n;
  const r = await create(alice, { name: '  my   build\tkey ', scopes: '[" files:* ","jobs:read","jobs:read"]', metadata: ' {"team":"ops","n":1.0} ' });
  aliceKey = r;
  assert.match(r.keyId, /^ak_[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}$/);
  assert.match(r.key, /^stdb_live_[0-9a-f]{64}$/);
  assert.equal(r.key, `stdb_live_${derive(SECRET, counter, r.createdAt)}`, 'HMAC(secret, counter, timestamp)');
  assert.equal(r.prefix, r.key.slice(0, 'stdb_live_'.length + 16));
  assert.equal(r.ownerSubject, hex(alice));
  assert.equal(r.name, 'my build key');
  assert.equal(r.scopesJson, '["files:*","jobs:read"]');
  assert.equal(r.metadataJson, '{"team":"ops","n":1}');
  assert.equal(r.status, 0, 'Active');
  assert.equal(r.expiresAt, undefined);
  const stored = (await sql(`SELECT * FROM api_key WHERE key_id = '${r.keyId}'`))[0];
  assert.equal(stored.hash, sha256(r.key));
  assert.ok(!JSON.stringify(stored).includes(r.key), 'raw key not stored');
  const usage = await usageFor(r.keyId);
  assert.deepEqual(usage.map((u) => [u.action, u.allowed, u.reason]), [['create', true, 'created']]);
  // Custom prefix and expiry.
  const t = await create(alice, { prefix: 'stdb_test', expires: 3600 });
  assert.match(t.key, /^stdb_test_[0-9a-f]{64}$/);
  assert.equal(micros(t.expiresAt) - t.createdAt, 3_600_000_000n);
});

await test('create: every field is validated', async () => {
  const cases = [
    [{ name: '   ' }, 'api_keys.invalid_name'],
    [{ name: 'n'.repeat(121) }, 'api_keys.invalid_name'],
    [{ scopes: '{}' }, 'api_keys.invalid_scopes_json'],
    [{ scopes: '[]' }, 'api_keys.invalid_scopes_json'],
    [{ scopes: 'nope' }, 'api_keys.invalid_scopes_json'],
    [{ scopes: '["ok",1]' }, 'api_keys.invalid_scopes_json'],
    [{ scopes: '["bad scope"]' }, 'api_keys.invalid_scopes_json'],
    [{ scopes: '["  "]' }, 'api_keys.invalid_scopes_json'],
    [{ scopes: JSON.stringify(['s'.repeat(129)]) }, 'api_keys.invalid_scopes_json'],
    [{ scopes: JSON.stringify(Array.from({ length: 129 }, (_, i) => `s${i}`)) }, 'api_keys.invalid_scopes_json'],
    [{ metadata: '[1]' }, 'api_keys.invalid_metadata_json'],
    [{ metadata: 'null' }, 'api_keys.invalid_metadata_json'],
    [{ metadata: '{bad' }, 'api_keys.invalid_metadata_json'],
    [{ metadata: JSON.stringify({ x: 'y'.repeat(8200) }) }, 'api_keys.invalid_metadata_json'],
    [{ expires: 0 }, 'api_keys.invalid_expires_in_seconds'],
    [{ expires: 315_360_001 }, 'api_keys.invalid_expires_in_seconds'],
    [{ prefix: '1abc' }, 'api_keys.invalid_key_prefix'],
    [{ prefix: 'a' }, 'api_keys.invalid_key_prefix'],
    [{ prefix: 'a-b' }, 'api_keys.invalid_key_prefix'],
    [{ prefix: 'a'.repeat(33) }, 'api_keys.invalid_key_prefix'],
  ];
  for (const [o, code] of cases) assert.equal(await createErr(alice, o), code, JSON.stringify(o).slice(0, 80));
  const blank = await create(alice, { metadata: '   ', scopes: '["*"]' });
  assert.equal(blank.metadataJson, undefined, 'blank metadata becomes none');
});

await test('views: my_api_keys is per caller and never shows secrets; admin views for admins only', async () => {
  const mine = await sql('SELECT * FROM my_api_keys', alice);
  assert.ok(mine.some((k) => k.key_id === aliceKey.keyId));
  assert.ok(mine.every((k) => k.owner_subject === hex(alice)));
  assert.ok(!('hash' in mine[0]) && !JSON.stringify(mine).includes(aliceKey.key));
  assert.equal((await sql('SELECT * FROM my_api_keys', bob)).length, 0);
  assert.equal((await sql('SELECT * FROM api_keys_admin', bob)).length, 0);
  assert.equal((await sql('SELECT * FROM api_key_usage_admin', bob)).length, 0);
  assert.ok((await sql('SELECT * FROM api_keys_admin')).some((k) => k.key_id === aliceKey.keyId));
  assert.ok((await sql('SELECT * FROM api_key_usage_admin')).length > 0);
});

await test('verify: allowed, scopes, actions, last_used_at, and which outcomes are recorded', async () => {
  const k = aliceKey;
  const ok = await verify(`  ${k.key} `, 'files:write');
  assert.deepEqual(ok, { allowed: true, reason: 'allowed', keyId: k.keyId, prefix: k.prefix, ownerSubject: hex(alice), scopesJson: k.scopesJson, metadataJson: k.metadataJson });
  assert.equal((await verify(k.key, 'admin:write')).reason, 'scope_denied');
  assert.equal((await verify(k.key)).allowed, true, 'no scope required');
  const tampered = k.prefix + 'f'.repeat(64 - 16);
  const bad = await verify(tampered === k.key ? k.prefix + '0'.repeat(48) : tampered, 'files:read');
  assert.deepEqual(bad, { allowed: false, reason: 'invalid_key', keyId: undefined, prefix: k.prefix, ownerSubject: undefined, scopesJson: undefined, metadataJson: undefined });
  const unknown = await verify('stdb_live_0000000000000000ffff');
  assert.equal(unknown.reason, 'unknown_key');
  assert.equal(unknown.prefix, 'stdb_live_0000000000000000');
  assert.deepEqual(await verify('missing-secret'), { allowed: false, reason: 'invalid_key', keyId: undefined, prefix: undefined, ownerSubject: undefined, scopesJson: undefined, metadataJson: undefined });
  assert.equal((await verify('stdb_live_short')).reason, 'invalid_key');
  assert.equal((await verify('x'.repeat(129))).reason, 'invalid_key');
  assert.equal(await procErr('demo_verify', [k.key, some('bad scope'), none]), 'api_keys.invalid_required_scope');
  assert.equal(await procErr('demo_verify', [k.key, none, some('  ')]), 'api_keys.invalid_action');
  await verify(k.key, undefined, 'upload');
  const usage = (await usageFor(k.keyId)).map((u) => [u.action, u.allowed, u.reason]);
  assert.deepEqual(usage.slice(1), [
    ['files:write', true, 'allowed'],
    ['admin:write', false, 'scope_denied'],
    ['verify', true, 'allowed'],
    ['files:read', false, 'invalid_key'],
    ['upload', true, 'allowed'],
  ], 'malformed and unknown keys are not recorded');
  const row = (await sql(`SELECT * FROM api_key WHERE key_id = '${k.keyId}'`))[0];
  assert.ok(row.last_used_at, 'last_used_at set');
});

await test('rotate: same id, new secret, old key stops working; owners and admins only', async () => {
  const k = await create(alice, { name: 'rotating', scopes: '["jobs:*"]' });
  assert.equal(await procErr('rotate_api_key', [k.keyId, none, none], bob), 'api_keys.not_authorized');
  assert.equal(await procErr('rotate_api_key', ['  ', none, none], alice), 'api_keys.invalid_key_id');
  assert.equal(await procErr('rotate_api_key', ['ak_missing', none, none], alice), 'api_keys.not_found');
  assert.equal(await procErr('rotate_api_key', [k.keyId, some(0), none], alice), 'api_keys.invalid_expires_in_seconds');
  const r = createResult(unwrap(await call('rotate_api_key', [k.keyId, some(60), none], alice)));
  assert.equal(r.keyId, k.keyId);
  assert.notEqual(r.key, k.key);
  assert.ok(micros(r.expiresAt) > 0n);
  assert.equal((await verify(k.key)).reason, 'unknown_key', 'old prefix is gone');
  assert.equal((await verify(r.key, 'jobs:run')).allowed, true);
  const admin = createResult(unwrap(await call('rotate_api_key', [k.keyId, none, none])));
  assert.equal(admin.ownerSubject, hex(alice), 'an admin can rotate any key');
  assert.deepEqual(admin.expiresAt, r.expiresAt, 'expiry kept when not given');
  assert.ok((await usageFor(k.keyId)).some((u) => u.action === 'rotate' && u.reason === 'rotated'));
});

await test('revoke: owner, admin-for-subject, idempotent; revoked keys fail and do not rotate', async () => {
  const k = await create(alice, { name: 'revoking' });
  assert.equal(await callErr('revoke_api_key', [k.keyId], bob), 'api_keys.not_authorized');
  assert.equal(await callErr('revoke_api_key', ['ak_missing'], alice), 'api_keys.not_found');
  await call('revoke_api_key', [k.keyId], alice);
  await call('revoke_api_key', [k.keyId], alice);
  assert.equal((await usageFor(k.keyId)).filter((u) => u.action === 'revoke').length, 1, 'revoking twice is a no-op');
  assert.equal((await verify(k.key)).reason, 'revoked');
  assert.equal(await procErr('rotate_api_key', [k.keyId, none, none], alice), 'api_keys.key_revoked');
  const k2 = await create(alice, { name: 'revoked by admin' });
  assert.equal(await callErr('revoke_api_key_for_subject', [k2.keyId, hex(alice)], bob), 'api_keys.not_authorized');
  await call('revoke_api_key_for_subject', [k2.keyId, hex(alice)]);
  assert.equal((await verify(k2.key)).reason, 'revoked');
});

await test('expiry: expired keys fail verification and cannot rotate', async () => {
  const k = await create(alice, { name: 'short-lived', expires: 1 });
  assert.equal((await verify(k.key)).allowed, true);
  await sleep(1100);
  assert.equal((await verify(k.key)).reason, 'expired');
  assert.equal(await procErr('rotate_api_key', [k.keyId, none, none], alice), 'api_keys.key_expired');
});

await test('limit: 50 active keys per owner (revoked and expired ones do not count); admins create for any subject', async () => {
  const subject = `service:${Date.now()}`;
  const mk = () => call('create_api_key_for_subject', [subject, 'svc', '["svc:*"]', none, none, none]);
  assert.equal(await procErr('create_api_key_for_subject', [subject, 'svc', '["svc:*"]', none, none, none], bob), 'api_keys.not_authorized');
  assert.equal(await procErr('create_api_key_for_subject', ['  ', 'svc', '["svc:*"]', none, none, none]), 'api_keys.invalid_owner_subject');
  const first = createResult(unwrap(await mk()));
  for (let i = 1; i < 50; i++) unwrap(await mk());
  assert.equal(await procErr('create_api_key_for_subject', [subject, 'svc', '["svc:*"]', none, none, none]), 'api_keys.active_key_limit_reached');
  await call('revoke_api_key_for_subject', [first.keyId, subject]);
  unwrap(await mk());
});

await test('usage retention: the sweep deletes rows older than the window only', async () => {
  const k = await create(bob, { name: 'sweepable' });
  const [row] = await usageFor(k.keyId);
  await call('demo_backdate_usage', [Number(row.usage_id), 3700]);
  const recent = await create(bob, { name: 'recent' });
  const deleted = unwrap(await call('demo_run_usage_sweep', []));
  assert.ok(deleted >= 1);
  assert.equal((await usageFor(k.keyId)).length, 0, 'old row swept');
  assert.equal((await usageFor(recent.keyId)).length, 1, 'recent row kept');
});

await test('admin reducers: add/remove admins, last admin protected, scheduled sweep rejects clients', async () => {
  const sid = { __identity__: '0x' + bob.identity };
  assert.equal(await callErr('add_api_keys_admin', [sid], bob), 'api_keys.not_authorized');
  await call('add_api_keys_admin', [sid]);
  assert.ok((await sql('SELECT * FROM api_keys_admin', bob)).length > 0, 'new admin sees keys');
  await call('remove_api_keys_admin', [sid]);
  assert.equal((await sql('SELECT * FROM api_keys_admin', bob)).length, 0);
  const me = (await sql('SELECT * FROM api_key_admin_identity'))[0].identity;
  assert.equal(await callErr('remove_api_keys_admin', [me]), 'api_keys.cannot_remove_last_admin');
  assert.ok(await callErr('api_keys_sweep', [[0, { Interval: [60_000_000] }]], bob), 'clients cannot run the scheduled sweep');
});

done();
