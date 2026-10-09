// @spacetimedb/auth port: every value from the real TS code (tests/vectors/auth.json: ES256
// keys, PEMs, key ids, JWTs, verification outcomes, scrypt hashes, derived secrets, UUIDv7s,
// PKCE, redirects, query parsing) plus the upstream test cases and the whole HTTP and reducer
// surface end to end, with a mock OAuth provider standing in for Google and GitHub.
import assert from 'node:assert/strict';
import { createHash, verify as nodeVerify } from 'node:crypto';
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { HOST, DB, call, callErr, procErr, sql, newIdentity, test, done, unwrap, some, none } from './lib.mjs';

const V = JSON.parse(readFileSync(new URL('./vectors/auth.json', import.meta.url)));
const BASE = `${HOST}/v1/database/${DB}/route`;
const opt = (v) => (v === undefined || v === null ? none : some(v));
const fromOpt = (o) => (o[0] === 0 ? o[1] : undefined);
const b64u = (s) => Buffer.from(s.replace(/-/g, '+').replace(/_/g, '/'), 'base64');
const jwtParts = (t) => t.split('.').slice(0, 2).map((p) => JSON.parse(b64u(p).toString()));
const sha256b64u = (s) => createHash('sha256').update(s).digest('base64url');

// ---------------------------------------------------------------------------
// Mock OAuth provider (Google-shaped at /, GitHub-shaped at /gh)
// ---------------------------------------------------------------------------
const mock = { profiles: new Map(), requests: [], ghUsers: new Map(), ghEmails: new Map(), tokenStatus: 200 };
const readBody = (req) => new Promise((r) => { let b = ''; req.on('data', (c) => (b += c)); req.on('end', () => r(b)); });
const server = createServer(async (req, res) => {
  const body = await readBody(req);
  const url = new URL(req.url, 'http://x');
  mock.requests.push({ method: req.method, path: url.pathname, headers: req.headers, body });
  const json = (status, v) => { res.writeHead(status, { 'content-type': 'application/json' }); res.end(JSON.stringify(v)); };
  const bearer = (req.headers.authorization ?? '').replace(/^Bearer /, '');
  if (req.method === 'POST' && (url.pathname === '/token' || url.pathname === '/gh/token')) {
    if (mock.tokenStatus !== 200) return json(mock.tokenStatus, { error: 'nope' });
    const form = new URLSearchParams(body);
    return json(200, { access_token: `at-${form.get('code')}`, refresh_token: `rt-${form.get('code')}`, expires_in: 3600, token_type: 'Bearer' });
  }
  if (url.pathname === '/userinfo') {
    const p = mock.profiles.get(bearer.slice(3));
    return p ? json(200, p) : json(401, { error: 'unknown token' });
  }
  if (url.pathname === '/gh/user') return json(200, mock.ghUsers.get(bearer.slice(3)) ?? {});
  if (url.pathname === '/gh/user/emails') return json(200, mock.ghEmails.get(bearer.slice(3)) ?? []);
  json(404, { error: 'not found' });
});
await new Promise((r) => server.listen(4310, '127.0.0.1', r));

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------
async function http(method, path, { body, token, cookie, ip, headers = {} } = {}) {
  const h = { ...headers };
  if (body !== undefined) h['content-type'] = 'application/json';
  if (token) h.authorization = `Bearer ${token}`;
  if (cookie) h.cookie = cookie;
  if (ip) h['x-real-ip'] = ip;
  const r = await fetch(`${BASE}${path}`, { method, headers: h, body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body), redirect: 'manual' });
  const text = await r.text();
  let json;
  try { json = JSON.parse(text); } catch {}
  return { status: r.status, json, text, headers: r.headers, cookies: r.headers.getSetCookie() };
}
const cookieValue = (cookies, name) => cookies.map((c) => c.split(';')[0]).find((c) => c.startsWith(`${name}=`))?.slice(name.length + 1);
let ipCounter = 0;
const freshIp = () => `198.51.100.${++ipCounter}`;
const signup = (email, password = 'correct horse battery', name) =>
  http('POST', '/auth/password/signup', { body: name === undefined ? { email, password } : { email, password, name }, ip: freshIp() });
const login = (email, password, ip = freshIp()) => http('POST', '/auth/password/login', { body: { email, password }, ip });
const link = async (who, token) => call('auth_link_connection', [token], who);
const mails = async (to) => (await sql(`SELECT * FROM demo_mail WHERE to = '${to}'`)).sort((a, b) => a.id - b.id);
const tokenFromMail = (m) => decodeURIComponent(m.text.match(/token=([^\s]+)/)[1]);

// ---------------------------------------------------------------------------
// Vectors
// ---------------------------------------------------------------------------
await test('keys: public key, PKCS#8/SPKI PEMs and RFC 7638 kid match TS (noble) byte for byte', async () => {
  for (const k of V.keys) {
    const [pub, privPem, pubPem, kid] = unwrap(await call('demo_keypair', [k.priv]));
    assert.equal(pub, k.pub);
    assert.equal(privPem, k.privateKeyPem);
    assert.equal(pubPem, k.publicKeyPem);
    assert.equal(kid, k.kid);
  }
  for (const e of V.external) {
    assert.equal(unwrap(await call('demo_private_key_from_pem', [e.pkcs8Pem])), e.priv, 'node:crypto PKCS#8 PEM parses');
    assert.equal(unwrap(await call('demo_public_key_from_pem', [e.spkiPem])), e.pub);
    const [, privPem, , kid] = unwrap(await call('demo_keypair', [e.priv]));
    assert.equal(privPem, e.normalizedPrivateKeyPem);
    assert.equal(kid, e.kid);
  }
  assert.match(await procErr('demo_private_key_from_pem', ['nope']), /PEM PRIVATE KEY block not found/);
  assert.match(await procErr('demo_public_key_from_pem', ['-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----']), /could not extract raw public key/);
});

await test('JWT: ES256 signatures are byte-identical to noble (RFC 6979, low-S), including JSON escaping', async () => {
  for (const j of V.jwts) {
    const c = j.claims;
    const token = unwrap(await call('demo_session_jwt', [j.priv, c.iss, c.sub, c.iat, c.exp, c.jti, opt(j.kid)]));
    assert.equal(token, j.token, `claims ${JSON.stringify(c)}`);
  }
});

await test(`JWT: verifyJwt outcomes match TS for ${V.verify.length} cases (tampering, high-S, expiry, nbf, iss/aud)`, async () => {
  for (const v of V.verify) {
    const got = JSON.parse(unwrap(await call('demo_verify_jwt', [v.pub, v.token, opt(v.opts.issuer), opt(v.opts.audience), v.opts.nowSeconds])));
    assert.deepEqual(got, v.result, v.name);
  }
});

await test('scrypt: hashPassword strings and verifyPassword outcomes match TS', async () => {
  for (const s of V.scrypt) assert.equal(unwrap(await call('demo_hash_password', [s.password, s.salt])), s.hash);
  for (const v of V.verifyPassword) assert.equal(unwrap(await call('demo_verify_password', [v.password, v.encoded])), v.ok, v.encoded);
  assert.equal(unwrap(await call('demo_dummy_hash', [])), V.dummyPasswordHash);
});

await test('secrets, UUIDv7, PKCE, safe redirects and query parsing match TS', async () => {
  for (const s of V.secrets) assert.equal(unwrap(await call('demo_derive_secret', [s.key, Number(s.counter), Number(s.micros)])), s.out);
  for (const u of V.uuids) assert.equal(unwrap(await call('demo_uuid', [Number(u.ms), u.rand])), u.out);
  for (const p of V.pkce) assert.equal(unwrap(await call('demo_pkce', [p.verifier])), p.challenge);
  for (const r of V.redirects) assert.equal(fromOpt(unwrap(await call('demo_safe_redirect', [r.value]))) ?? null, r.result, JSON.stringify(r.value).slice(0, 60));
  for (const q of V.queries) assert.deepEqual(JSON.parse(unwrap(await call('demo_parse_query', [q.uri]))), q.result, q.uri);
});

// ---------------------------------------------------------------------------
// Configuration and admins
// ---------------------------------------------------------------------------
const KEY = V.external[0];
await test('config: admin only, a signing key is required, a bad PEM is rejected, the public key is served', async () => {
  const outsider = await newIdentity();
  const args = (pem) => [BASE, some(BASE), none, none, opt(pem), some('mock-client'), some('mock-secret'), some('gh-client'), some('gh-secret')];
  assert.equal(await callErr('set_auth_config', args(KEY.pkcs8Pem), outsider), 'auth.not_authorized');
  assert.match(await procErr('get_auth_public_key', []), /auth\.config_missing/);
  assert.equal(await callErr('set_auth_config', args(undefined)), 'auth.signing_key_required');
  assert.match(await callErr('set_auth_config', args('not a pem')), /^auth\.invalid_private_key_pem:PEM PRIVATE KEY block not found/);
  await call('set_auth_config', args(KEY.pkcs8Pem));
  // A later call without a PEM keeps the stored key.
  await call('set_auth_config', args(undefined));
  const [pem, kid, iss] = unwrap(await call('get_auth_public_key', []));
  assert.equal(kid, KEY.kid);
  assert.equal(iss, BASE);
  assert.equal(unwrap(await call('demo_public_key_from_pem', [pem])), KEY.pub);
});

await test('admins: only admins add admins; the last admin cannot be removed', async () => {
  const a = await newIdentity();
  const sid = { __identity__: '0x' + a.identity };
  assert.equal(await callErr('add_auth_admin', [sid], a), 'auth.not_authorized');
  await call('add_auth_admin', [sid]);
  assert.equal((await sql('SELECT * FROM auth_admin_identity')).length, 2);
  await call('remove_auth_admin', [sid], a);
  const [{ identity }] = await sql('SELECT * FROM auth_admin_identity');
  assert.equal(await callErr('remove_auth_admin', [identity]), 'auth.last_admin');
});

// ---------------------------------------------------------------------------
// Password sign-up, login, sessions
// ---------------------------------------------------------------------------
let ada;
await test('signup: validates input, normalizes email, issues a signed session JWT and cookie', async () => {
  assert.equal((await http('POST', '/auth/password/signup', { body: 'not json' })).json.error, 'invalid_request');
  assert.equal((await signup('x@example.com', 'short')).json.error, 'password_too_short');
  assert.equal((await signup('x@example.com', 'p'.repeat(1025))).json.error, 'password_too_long');
  assert.equal((await signup('   ', 'long enough pw')).json.error, 'invalid_email');
  assert.equal((await signup('x@example.com', 'long enough pw', 'n'.repeat(129))).json.error, 'name_too_long');
  const r = await signup('  Ada@Example.COM ', 'correct horse battery', 'Ada');
  assert.equal(r.status, 200, r.text);
  assert.equal(r.json.user.email, 'ada@example.com');
  assert.match(r.json.user.userId, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  const cookie = r.cookies.find((c) => c.startsWith('stdb_auth='));
  assert.match(cookie, /^stdb_auth=[^;]+; Path=\/; Max-Age=604800; HttpOnly; SameSite=Lax$/);
  const [header, claims] = jwtParts(r.json.token);
  assert.deepEqual(header, { alg: 'ES256', typ: 'JWT', kid: KEY.kid });
  assert.deepEqual(Object.keys(claims), ['iss', 'sub', 'aud', 'iat', 'exp', 'jti']);
  assert.equal(claims.iss, BASE);
  assert.equal(claims.aud, BASE);
  assert.equal(claims.sub, r.json.user.userId);
  assert.equal(claims.exp - claims.iat, 604800);
  const [h, p, s] = r.json.token.split('.');
  assert.ok(nodeVerify('sha256', Buffer.from(`${h}.${p}`), { key: KEY.spkiPem, dsaEncoding: 'ieee-p1363' }, b64u(s)), 'node verifies the signature');
  const [session] = await sql(`SELECT * FROM auth_session WHERE session_id = '${claims.jti}'`);
  assert.equal(session.user_id, claims.sub);
  const [acct] = await sql(`SELECT * FROM auth_account WHERE user_id = '${claims.sub}'`);
  assert.match(fromOpt(acct.password_hash), /^scrypt\$16384\$8\$1\$/);
  assert.equal((await signup('ada@example.com')).json.error, 'email_taken');
  ada = { userId: r.json.user.userId, token: r.json.token, email: 'ada@example.com', password: 'correct horse battery' };
});

await test('login: right password succeeds, wrong or unknown fail alike; me works by bearer and cookie', async () => {
  const ok = await login(ada.email, ada.password);
  assert.equal(ok.status, 200, ok.text);
  assert.equal(ok.json.userId, ada.userId);
  assert.equal((await login(ada.email, 'wrong password')).json.error, 'invalid_credentials');
  assert.equal((await login('nobody@example.com', 'whatever pw')).json.error, 'invalid_credentials');
  assert.equal((await login(ada.email, 'p'.repeat(1025))).json.error, 'invalid_credentials');
  const me = await http('GET', '/auth/me', { token: ok.json.token });
  assert.deepEqual(me.json.user, { userId: ada.userId, email: ada.email, emailVerified: false, name: 'Ada' });
  const byCookie = await http('GET', '/auth/me', { cookie: `stdb_auth=${ok.json.token}` });
  assert.equal(byCookie.json.user.userId, ada.userId);
  assert.equal((await http('GET', '/auth/me')).json.error, 'unauthenticated');
  assert.equal((await http('GET', '/auth/me', { token: ok.json.token.slice(0, -2) + 'AA' })).json.error, 'unauthenticated');
  assert.deepEqual((await http('GET', '/whoami-http', { token: ok.json.token })).json, { userId: ada.userId });
  assert.deepEqual((await http('GET', '/whoami-http')).json, { userId: null });
});

let adaConn;
await test('connection binding: link_connection, whoami, my_auth_user, notes owned by the caller', async () => {
  adaConn = await newIdentity();
  assert.equal(await callErr('create_note', ['t', 'b'], adaConn), 'auth.not_authenticated');
  assert.equal(await callErr('auth_link_connection', ['garbage'], adaConn), 'auth.invalid_token');
  await link(adaConn, ada.token);
  const [userId, sender] = unwrap(await call('auth_whoami', [], adaConn));
  assert.equal(fromOpt(userId), ada.userId);
  assert.equal(sender, adaConn.identity.replace(/^0x/, '').toLowerCase());
  const rows = await sql('SELECT * FROM my_auth_user', adaConn);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].user_id, ada.userId);
  assert.equal((await sql('SELECT * FROM my_auth_user', await newIdentity())).length, 0);
  await call('create_note', ['hello', 'world'], adaConn);
  const notes = await sql('SELECT * FROM my_notes', adaConn);
  assert.equal(notes.length, 1);
  assert.equal(notes[0].title, 'hello');
  const eve = await newIdentity();
  const eveToken = (await signup('eve@example.com')).json.token;
  await link(eve, eveToken);
  assert.equal(await callErr('delete_note', [notes[0].note_id], eve), 'note.not_owner');
});

await test('profile: trims, empty clears, length limits', async () => {
  await call('auth_update_profile', [some('  Ada L.  '), some('https://img/a.png')], adaConn);
  let [u] = await sql(`SELECT * FROM auth_user WHERE user_id = '${ada.userId}'`);
  assert.equal(fromOpt(u.name), 'Ada L.');
  await call('auth_update_profile', [none, some('')], adaConn);
  [u] = await sql(`SELECT * FROM auth_user WHERE user_id = '${ada.userId}'`);
  assert.equal(fromOpt(u.name), 'Ada L.');
  assert.equal(fromOpt(u.image), undefined);
  assert.equal(await callErr('auth_update_profile', [some('n'.repeat(65)), none], adaConn), 'auth.name_too_long:max=64');
  assert.equal(await callErr('auth_update_profile', [none, some('i'.repeat(2049))], adaConn), 'auth.image_too_long:max=2048');
  assert.equal(await callErr('auth_update_profile', [some('x'), none], await newIdentity()), 'auth.not_authenticated');
});

await test('sessions: list (newest first, current marked), revoke own, not others; admin revoke', async () => {
  const second = await login(ada.email, ada.password);
  const [sessions] = unwrap(await call('auth_list_my_sessions', [], adaConn));
  assert.ok(sessions.length >= 2);
  const current = sessions.filter((s) => s[5]);
  assert.equal(current.length, 1);
  assert.equal(current[0][0], jwtParts(ada.token)[1].jti);
  for (let i = 1; i < sessions.length; i++) assert.ok(BigInt(sessions[i - 1][2][0]) >= BigInt(sessions[i][2][0]), 'newest first');
  const secondId = jwtParts(second.json.token)[1].jti;
  const eve = await newIdentity();
  await link(eve, (await login('eve@example.com', 'correct horse battery')).json.token);
  assert.equal(await callErr('auth_revoke_my_session', [secondId], eve), 'auth.session_not_owned');
  await call('auth_revoke_my_session', [secondId], adaConn);
  assert.equal((await http('GET', '/auth/me', { token: second.json.token })).json.error, 'unauthenticated');
  assert.equal(unwrap(await call('auth_list_my_sessions', [], await newIdentity()))[0].length, 0);
  const third = await login(ada.email, ada.password);
  assert.equal(await callErr('auth_revoke_session', [jwtParts(third.json.token)[1].jti], eve), 'auth.not_authorized');
  await call('auth_revoke_session', [jwtParts(third.json.token)[1].jti]);
  assert.equal((await http('GET', '/auth/me', { token: third.json.token })).status, 401);
});

await test('refresh moves bindings to the new session; logout deletes the session and its bindings', async () => {
  const r = await http('POST', '/auth/session/refresh', { token: ada.token });
  assert.equal(r.status, 200, r.text);
  assert.ok(r.json.token !== ada.token);
  assert.equal(r.json.sessionExpiresAt, jwtParts(r.json.token)[1].exp);
  assert.equal((await http('GET', '/auth/me', { token: ada.token })).status, 401, 'old session is gone');
  assert.equal(fromOpt(unwrap(await call('auth_whoami', [], adaConn))[0]), ada.userId, 'binding moved');
  const [binding] = await sql(`SELECT * FROM auth_connection_binding WHERE user_id = '${ada.userId}'`);
  assert.equal(binding.session_id, jwtParts(r.json.token)[1].jti);
  ada.token = r.json.token;
  const out = await http('POST', '/auth/logout', { token: ada.token });
  assert.deepEqual(out.json, { ok: true });
  assert.match(out.cookies[0], /^stdb_auth=; Path=\/; Max-Age=0; HttpOnly; SameSite=Lax$/);
  assert.equal(unwrap(await call('auth_whoami', [], adaConn))[0][0], 1, 'binding removed');
  assert.equal((await http('POST', '/auth/session/refresh', { token: ada.token })).json.error, 'unauthenticated');
  ada.token = (await login(ada.email, ada.password)).json.token;
  await link(adaConn, ada.token);
});

await test('rate limits: login per email (10/5 min) with 429 headers; signup per email (5/h)', async () => {
  await signup('rl@example.com');
  const ip = freshIp();
  for (let i = 0; i < 10; i++) assert.equal((await login('rl@example.com', 'wrong password', ip)).status, 401);
  const blocked = await login('RL@example.com', 'correct horse battery', freshIp());
  assert.equal(blocked.status, 429);
  assert.equal(blocked.json.error, 'rate_limited');
  assert.ok(Number(blocked.headers.get('retry-after')) > 0);
  assert.equal(blocked.headers.get('x-ratelimit-limit'), '10');
  assert.equal(blocked.headers.get('x-ratelimit-remaining'), '0');
  assert.ok(Number(blocked.headers.get('x-ratelimit-reset')) > Date.now() / 1000);
  for (let i = 0; i < 5; i++) assert.notEqual((await signup('limit@example.com')).status, 429);
  assert.equal((await signup('limit@example.com')).status, 429);
});

// ---------------------------------------------------------------------------
// Email verification and password reset
// ---------------------------------------------------------------------------
await test('email verification: request needs a session, mails a link, the link verifies and redirects', async () => {
  assert.equal((await http('POST', '/auth/email/verify-request')).json.error, 'unauthenticated');
  const r = await http('POST', '/auth/email/verify-request', { token: ada.token, ip: freshIp() });
  assert.deepEqual(r.json, { ok: true });
  const [mail] = (await mails(ada.email)).slice(-1);
  assert.equal(mail.subject, 'Verify your email for Notes');
  assert.ok(mail.text.startsWith(`Click to verify your email:\n\n${BASE}/auth/email/verify?token=`));
  const token = tokenFromMail(mail);
  assert.equal((await http('GET', '/auth/email/verify')).json.error, 'missing_token');
  assert.equal((await http('GET', '/auth/email/verify?token=nope')).json.error, 'bad_token');
  const v = await http('GET', `/auth/email/verify?token=${encodeURIComponent(token)}`);
  assert.equal(v.status, 302);
  assert.equal(v.headers.get('location'), '/?verified=1');
  assert.equal((await http('GET', '/auth/me', { token: ada.token })).json.user.emailVerified, true);
  assert.equal((await http('GET', `/auth/email/verify?token=${encodeURIComponent(token)}`)).json.error, 'bad_token', 'single use');
  assert.deepEqual((await http('POST', '/auth/email/verify-request', { token: ada.token })).json, { ok: true, alreadyVerified: true });
});

await test('password reset: always 200, mails a link, resets, verifies the address and revokes every session', async () => {
  assert.deepEqual((await http('POST', '/auth/password/forgot', { body: { email: 'nobody@example.com' }, ip: freshIp() })).json, { ok: true });
  assert.equal((await mails('nobody@example.com')).length, 0);
  const bob = (await signup('bob@example.com')).json;
  const bobConn = await newIdentity();
  await link(bobConn, bob.token);
  assert.deepEqual((await http('POST', '/auth/password/forgot', { body: { email: 'BOB@example.com' }, ip: freshIp() })).json, { ok: true });
  const [mail] = (await mails('bob@example.com')).slice(-1);
  assert.equal(mail.subject, 'Reset your password for Notes');
  const token = tokenFromMail(mail);
  const reset = (body) => http('POST', '/auth/password/reset', { body, ip: freshIp() });
  assert.equal((await reset({ token, newPassword: 'short' })).json.error, 'password_too_short');
  assert.equal((await reset({ token: 'x'.repeat(257), newPassword: 'new password 1' })).json.error, 'bad_token');
  assert.equal((await reset({ token: 'unknown', newPassword: 'new password 1' })).json.error, 'bad_token');
  assert.deepEqual((await reset({ token, newPassword: 'new password 1' })).json, { ok: true });
  assert.equal((await reset({ token, newPassword: 'new password 2' })).json.error, 'bad_token', 'single use');
  assert.equal((await http('GET', '/auth/me', { token: bob.token })).status, 401, 'sessions revoked');
  assert.equal(unwrap(await call('auth_whoami', [], bobConn))[0][0], 1, 'bindings removed');
  assert.equal((await login('bob@example.com', 'correct horse battery')).status, 401);
  const again = await login('bob@example.com', 'new password 1');
  assert.equal(again.status, 200);
  assert.equal((await http('GET', '/auth/me', { token: again.json.token })).json.user.emailVerified, true);
});

// ---------------------------------------------------------------------------
// OAuth
// ---------------------------------------------------------------------------
async function oauthStart(provider, redirectTo) {
  const r = await http('GET', `/auth/${provider}/start${redirectTo === undefined ? '' : `?redirectTo=${encodeURIComponent(redirectTo)}`}`, { ip: freshIp() });
  if (r.status !== 302) return { r };
  const loc = new URL(r.headers.get('location'));
  return { r, loc, state: loc.searchParams.get('state'), cookie: `stdb_auth_oauth_state=${cookieValue(r.cookies, 'stdb_auth_oauth_state')}` };
}
const callback = (provider, code, state, cookie) =>
  http('GET', `/auth/${provider}/callback?code=${encodeURIComponent(code)}&state=${encodeURIComponent(state)}`, { cookie });

await test('OAuth (Google-shaped): PKCE start, state bound to the browser, code exchange, new user, session', async () => {
  const { r, loc, state, cookie } = await oauthStart('mock', '/home?tab=1');
  assert.equal(r.status, 302, r.text);
  assert.equal(loc.origin + loc.pathname, 'http://127.0.0.1:4310/authorize');
  assert.deepEqual([...loc.searchParams.keys()], ['client_id', 'redirect_uri', 'response_type', 'scope', 'state', 'code_challenge', 'code_challenge_method', 'access_type', 'prompt']);
  assert.equal(loc.searchParams.get('client_id'), 'mock-client');
  assert.equal(loc.searchParams.get('redirect_uri'), `${BASE}/auth/mock/callback`);
  assert.equal(loc.searchParams.get('scope'), 'openid email profile');
  assert.match(r.cookies[0], /^stdb_auth_oauth_state=[^;]+; Path=\/; Max-Age=600; HttpOnly; SameSite=Lax$/);
  mock.profiles.set('c1', { sub: 'g-1', email: 'Grace@Example.com', email_verified: true, name: 'Grace', picture: 'https://img/g.png' });
  assert.equal((await callback('mock', 'c1', state, undefined)).json.error, 'bad_state', 'no state cookie');
  assert.equal((await callback('mock', 'c1', state, 'stdb_auth_oauth_state=other')).json.error, 'bad_state');
  mock.requests.length = 0;
  const cb = await callback('mock', 'c1', state, cookie);
  assert.equal(cb.status, 302, cb.text);
  assert.equal(cb.headers.get('location'), '/home?tab=1');
  const tokenReq = mock.requests.find((q) => q.path === '/token');
  const form = Object.fromEntries(new URLSearchParams(tokenReq.body));
  assert.deepEqual(Object.keys(form), ['grant_type', 'code', 'client_id', 'client_secret', 'redirect_uri', 'code_verifier']);
  assert.equal(form.client_secret, 'mock-secret');
  assert.equal(sha256b64u(form.code_verifier), loc.searchParams.get('code_challenge'), 'PKCE S256');
  assert.equal(tokenReq.headers['content-type'], 'application/x-www-form-urlencoded');
  const session = cookieValue(cb.cookies, 'stdb_auth');
  assert.match(cb.cookies[1], /^stdb_auth_oauth_state=; Path=\/; Max-Age=0/);
  const me = await http('GET', '/auth/me', { cookie: `stdb_auth=${session}` });
  assert.deepEqual(me.json.user, { userId: me.json.user.userId, email: 'grace@example.com', emailVerified: true, name: 'Grace', image: 'https://img/g.png' });
  const [acct] = await sql(`SELECT * FROM auth_account WHERE provider_account_id = 'g-1'`);
  assert.equal(acct.provider_id, 'mock');
  assert.equal(fromOpt(acct.access_token), 'at-c1');
  assert.equal(fromOpt(acct.refresh_token), 'rt-c1');
  assert.ok(fromOpt(acct.access_token_expires_at));
  assert.equal((await callback('mock', 'c1', state, cookie)).json.error, 'bad_state', 'state is single use');
  // Signing in again updates the same account.
  const again = await oauthStart('mock');
  mock.profiles.set('c2', { sub: 'g-1', email: 'grace@example.com', email_verified: true });
  const cb2 = await callback('mock', 'c2', again.state, again.cookie);
  assert.equal(cb2.headers.get('location'), '/');
  const [acct2] = await sql(`SELECT * FROM auth_account WHERE provider_account_id = 'g-1'`);
  assert.equal(fromOpt(acct2.access_token), 'at-c2');
});

await test('OAuth: unsafe redirects, provider failures, incomplete profiles, and verified-only account linking', async () => {
  assert.equal((await oauthStart('mock', 'https://evil.example')).r.json.error, 'invalid_redirect');
  assert.equal((await oauthStart('mock', '//evil.example')).r.json.error, 'invalid_redirect');
  assert.equal((await http('GET', '/auth/mock/callback?code=x')).json.error, 'missing_code_or_state');
  assert.equal((await http('GET', `/auth/mock/callback?code=x&state=${'s'.repeat(257)}`)).json.error, 'invalid_code_or_state');
  let s = await oauthStart('mock');
  mock.tokenStatus = 400;
  assert.equal((await callback('mock', 'c3', s.state, s.cookie)).json.error, 'token_exchange_failed');
  mock.tokenStatus = 200;
  s = await oauthStart('mock');
  assert.equal((await callback('mock', 'unknown-code', s.state, s.cookie)).json.error, 'userinfo_failed:401');
  s = await oauthStart('mock');
  mock.profiles.set('c4', { sub: 'g-4', email: '' });
  assert.equal((await callback('mock', 'c4', s.state, s.cookie)).json.error, 'incomplete_profile');
  // An unverified local account is never linked to an OAuth identity.
  await signup('carol@example.com');
  s = await oauthStart('mock');
  mock.profiles.set('c5', { sub: 'g-5', email: 'carol@example.com', email_verified: true });
  assert.equal((await callback('mock', 'c5', s.state, s.cookie)).json.error, 'account_link_required');
  // Ada verified her address, and the provider did too: linked to the same user.
  s = await oauthStart('mock');
  mock.profiles.set('c6', { sub: 'g-6', email: 'ada@example.com', email_verified: true });
  const cb = await callback('mock', 'c6', s.state, s.cookie);
  assert.equal(cb.status, 302, cb.text);
  const me = await http('GET', '/auth/me', { cookie: `stdb_auth=${cookieValue(cb.cookies, 'stdb_auth')}` });
  assert.equal(me.json.user.userId, ada.userId);
  // ...but not when the provider hasn't verified it.
  s = await oauthStart('mock');
  mock.profiles.set('c7', { sub: 'g-7', email: 'ada@example.com', email_verified: false });
  assert.equal((await callback('mock', 'c7', s.state, s.cookie)).json.error, 'account_link_required');
});

await test('OAuth (GitHub-shaped): no PKCE, profile from /user + the primary verified /user/emails', async () => {
  const { r, loc, state, cookie } = await oauthStart('mockgh');
  assert.equal(r.status, 302, r.text);
  assert.equal(loc.searchParams.get('code_challenge'), null);
  assert.equal(loc.searchParams.get('scope'), 'read:user user:email');
  assert.equal(loc.searchParams.get('client_id'), 'gh-client');
  mock.ghUsers.set('h1', { id: 4242, login: 'octo', avatar_url: 'https://img/o.png' });
  mock.ghEmails.set('h1', [
    { email: 'unverified@example.com', primary: true, verified: false },
    { email: 'second@example.com', primary: false, verified: true },
    { email: 'Octo@Example.com', primary: true, verified: true },
  ]);
  mock.requests.length = 0;
  const cb = await callback('mockgh', 'h1', state, cookie);
  assert.equal(cb.status, 302, cb.text);
  const form = Object.fromEntries(new URLSearchParams(mock.requests.find((q) => q.path === '/gh/token').body));
  assert.equal(form.code_verifier, undefined);
  const userReq = mock.requests.find((q) => q.path === '/gh/user');
  assert.equal(userReq.headers['x-github-api-version'], '2022-11-28');
  assert.equal(userReq.headers.authorization, 'Bearer at-h1');
  const me = await http('GET', '/auth/me', { cookie: `stdb_auth=${cookieValue(cb.cookies, 'stdb_auth')}` });
  assert.deepEqual(me.json.user, { userId: me.json.user.userId, email: 'octo@example.com', emailVerified: true, name: 'octo', image: 'https://img/o.png' });
  const [acct] = await sql(`SELECT * FROM auth_account WHERE provider_account_id = '4242'`);
  assert.equal(acct.provider_id, 'mockgh');
});

await test('built-in providers: unconfigured client ids fail cleanly; scheduled sweep is scheduler-only', async () => {
  // google/github use the configured ids, so start works and points at the real endpoints.
  const g = await oauthStart('google');
  assert.equal(g.loc.origin + g.loc.pathname, 'https://accounts.google.com/o/oauth2/v2/auth');
  const gh = await oauthStart('github');
  assert.equal(gh.loc.origin + gh.loc.pathname, 'https://github.com/login/oauth/authorize');
  const err = await callErr('auth_sweep', [[1, { Interval: [60000000] }]]);
  assert.ok(err !== null, 'callers other than the scheduler are rejected');
});

server.close();
done();
