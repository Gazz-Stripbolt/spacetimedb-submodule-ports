// Generate tests/vectors/auth.json from the real @spacetimedb/auth code (src/ at the pinned
// commit 37ee2ad) and the same noble libraries it uses: ES256 keys, PEMs, key ids, JWTs,
// scrypt hashes, derived secrets, UUIDv7s, PKCE challenges and the request-trust helpers.
// The Rust and C# ports must reproduce every value exactly.
//
//   UPSTREAM=<SpacetimeDB checkout at 37ee2ad>/spacetime-auth-ts npx tsx tools/gen-auth-vectors.mjs
//
// Run it from a directory where @noble/curves@2, @noble/hashes@2 and @spacetimedb/crypto (the
// spacetime-crypto-ts package) resolve, with "type": "module". src/handlers/http.ts imports
// SyncResponse from spacetimedb/server, whose bundle uses `using` (not in Node 22): map that
// import to a stub with a tsconfig `paths` entry; nothing here constructs a response.
import { writeFileSync } from 'node:fs';
import { generateKeyPairSync } from 'node:crypto';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { p256 } from '@noble/curves/nist.js';

const upstream = process.env.UPSTREAM;
if (!upstream) throw new Error('set UPSTREAM to the spacetime-auth-ts package directory');
const src = (f) => import(pathToFileURL(join(upstream, 'src', f)).href);
const { hashPassword, verifyPassword, DUMMY_PASSWORD_HASH, deriveSecret, uuidV7, pkceChallenge, base64Encode, base64UrlEncode } = await src('crypto.ts');
const { fromPrivateKeyBytes, privateKeyFromPem, publicKeyFromPem } = await src('keys.ts');
const { signJwt, verifyJwt } = await src('jwt.ts');
const { safeRedirectPath } = await src('request-trust.ts');
const { parseQueryString } = await src('handlers/http.ts');

const hex = (b) => Buffer.from(b).toString('hex');
const unhex = (h) => new Uint8Array(Buffer.from(h, 'hex'));
// Deterministic bytes (mulberry32), so the vectors are reproducible.
let seed = 0xa07;
const rand = () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const bytes = (n) => Uint8Array.from({ length: n }, () => Math.floor(rand() * 256));
const privKey = () => {
  for (;;) {
    const k = bytes(32);
    try {
      p256.getPublicKey(k);
      return k;
    } catch {}
  }
};

const keys = [];
for (let i = 0; i < 6; i++) {
  const k = i === 0 ? unhex('01'.repeat(32)) : privKey();
  const kp = fromPrivateKeyBytes(k);
  keys.push({ priv: hex(k), pub: hex(kp.publicKey), privateKeyPem: kp.privateKeyPem, publicKeyPem: kp.publicKeyPem, kid: kp.kid });
}
// Keys generated outside the module (node:crypto / OpenSSL-style PKCS#8 and SPKI).
const external = [];
for (let i = 0; i < 3; i++) {
  const { privateKey, publicKey } = generateKeyPairSync('ec', {
    namedCurve: 'P-256',
    privateKeyEncoding: { type: 'pkcs8', format: 'pem' },
    publicKeyEncoding: { type: 'spki', format: 'pem' },
  });
  const raw = privateKeyFromPem(privateKey);
  const kp = fromPrivateKeyBytes(raw);
  external.push({ pkcs8Pem: privateKey, spkiPem: publicKey, priv: hex(raw), pub: hex(publicKeyFromPem(publicKey)), normalizedPrivateKeyPem: kp.privateKeyPem, kid: kp.kid });
}

const strings = ['https://auth.example.com', 'http://127.0.0.1:3000/v1/database/x/route', 'iss "quoted" \\ back', 'ünïcødé ✓ 🚀', 'ctl \u0001\u001f\n\t', '</script>'];
const jwts = [];
for (let i = 0; i < 12; i++) {
  const key = keys[i % keys.length];
  const iss = strings[i % strings.length];
  const iat = 1_700_000_000 + i * 977;
  const claims = { iss, sub: `user-${i}`, aud: iss, iat, exp: iat + 3600 * (i + 1), jti: base64UrlEncode(bytes(32)) };
  jwts.push({ priv: key.priv, kid: i % 4 === 3 ? null : key.kid, claims, token: signJwt(unhex(key.priv), claims, i % 4 === 3 ? undefined : key.kid) });
}

// verifyJwt outcomes, including tampering, a high-S twin of a valid signature, and claim checks.
const N = p256.Point.CURVE().n;
const b64u = (b) => base64UrlEncode(b);
const verify = [];
const addV = (name, pub, token, opts) => verify.push({ name, pub, token, opts, result: verifyJwt(unhex(pub), token, opts) });
{
  const k = keys[1];
  const now = 1_800_000_000;
  const good = signJwt(unhex(k.priv), { iss: 'https://a.example', sub: 'u1', aud: 'https://a.example', iat: now - 10, exp: now + 3600, jti: 's1' }, k.kid);
  addV('valid', k.pub, good, { issuer: 'https://a.example', nowSeconds: now });
  addV('valid with audience', k.pub, good, { issuer: 'https://a.example', audience: 'https://a.example', nowSeconds: now });
  addV('wrong key', keys[2].pub, good, { nowSeconds: now });
  addV('wrong issuer', k.pub, good, { issuer: 'https://b.example', nowSeconds: now });
  addV('wrong audience', k.pub, good, { audience: 'https://b.example', nowSeconds: now });
  addV('expired', k.pub, good, { nowSeconds: now + 3600 + 61 });
  addV('expired within tolerance', k.pub, good, { nowSeconds: now + 3600 + 60 });
  const nbf = signJwt(unhex(k.priv), { iss: 'x', sub: 'y', iat: now, exp: now + 100, nbf: now + 500 }, k.kid);
  addV('not yet valid', k.pub, nbf, { nowSeconds: now });
  addV('nbf within tolerance', k.pub, nbf, { nowSeconds: now + 440 });
  const arrAud = signJwt(unhex(k.priv), { iss: 'x', sub: 'y', aud: ['p', 'q'], iat: now, exp: now + 100 }, k.kid);
  addV('array audience', k.pub, arrAud, { audience: 'q', nowSeconds: now });
  addV('array audience miss', k.pub, arrAud, { audience: 'r', nowSeconds: now });
  const [h, p, s] = good.split('.');
  const sig = Buffer.from(s.replace(/-/g, '+').replace(/_/g, '/'), 'base64');
  const r = sig.subarray(0, 32);
  const sv = BigInt('0x' + sig.subarray(32).toString('hex'));
  const highS = Buffer.from((N - sv).toString(16).padStart(64, '0'), 'hex');
  addV('high-S twin', k.pub, `${h}.${p}.${b64u(Buffer.concat([r, highS]))}`, { nowSeconds: now });
  addV('tampered payload', k.pub, `${h}.${b64u(Buffer.from('{"iss":"x","sub":"evil","iat":1,"exp":9999999999}'))}.${s}`, { nowSeconds: now });
  addV('two parts', k.pub, `${h}.${p}`, { nowSeconds: now });
  addV('four parts', k.pub, `${h}.${p}.${s}.x`, { nowSeconds: now });
  addV('padded base64', k.pub, `${h}.${p}.${s}==`, { nowSeconds: now });
  addV('plus in signature', k.pub, `${h}.${p}.${s.replace(/[-_]/, '+')}`, { nowSeconds: now });
  addV('short signature', k.pub, `${h}.${p}.${b64u(sig.subarray(0, 63))}`, { nowSeconds: now });
  addV('payload not json', k.pub, `${h}.${b64u(Buffer.from('not json'))}.${s}`, { nowSeconds: now });
  addV('alg none', k.pub, `${b64u(Buffer.from('{"alg":"none","typ":"JWT"}'))}.${p}.${s}`, { nowSeconds: now });
  addV('alg HS256', k.pub, `${b64u(Buffer.from('{"alg":"HS256","typ":"JWT"}'))}.${p}.${s}`, { nowSeconds: now });
  addV('empty', k.pub, '', { nowSeconds: now });
  addV('no exp', k.pub, signJwt(unhex(k.priv), { iss: 'x', sub: 'y', iat: 1 }, k.kid), { nowSeconds: now });
  addV('string exp is ignored', k.pub, signJwt(unhex(k.priv), { iss: 'x', sub: 'y', iat: 1, exp: '5' }, k.kid), { nowSeconds: now });
  addV('fractional exp', k.pub, signJwt(unhex(k.priv), { iss: 'x', sub: 'y', iat: 1, exp: now - 60.5 }, k.kid), { nowSeconds: now });
}

const scrypt = [];
for (const [pw, salt] of [['correct horse battery staple', '07'.repeat(16)], ['p', '00'.repeat(16)], ['pässwörd ✓ 🚀', hex(bytes(16))], ['x'.repeat(1024), hex(bytes(16))]]) {
  scrypt.push({ password: pw, salt, hash: hashPassword(pw, unhex(salt)) });
}
const verifyPw = [];
const addPw = (password, encoded) => verifyPw.push({ password, encoded, ok: verifyPassword(password, encoded) });
addPw(scrypt[0].password, scrypt[0].hash);
addPw('wrong', scrypt[0].hash);
addPw(scrypt[2].password, scrypt[2].hash);
addPw(scrypt[0].password, DUMMY_PASSWORD_HASH);
addPw('', DUMMY_PASSWORD_HASH);
addPw('a', 'bcrypt$1$2$3$AAAA$AAAA');
addPw('a', 'scrypt$16384$8$1$AAAA');
addPw('a', 'scrypt$16384$8$1$!!!!$AAAA');
addPw('a', `scrypt$1024$8$1$${base64Encode(bytes(16))}$${base64Encode(bytes(32))}`);
{
  // A cheaper hash with different parameters still verifies (parameters come from the string).
  const { scrypt: nobleScrypt } = await import('@noble/hashes/scrypt.js');
  const salt = bytes(16);
  const h = nobleScrypt(new TextEncoder().encode('cheap'), salt, { N: 1024, r: 4, p: 2, dkLen: 24 });
  addPw('cheap', `scrypt$1024$4$2$${base64Encode(salt)}$${base64Encode(h)}`);
  addPw('cheaper', `scrypt$1024$4$2$${base64Encode(salt)}$${base64Encode(h)}`);
}

const secrets = [];
for (let i = 0; i < 6; i++) {
  const key = i === 0 ? unhex(keys[0].priv) : bytes(32);
  const counter = BigInt(i * 1_000_003);
  const micros = 1_760_000_000_000_000n + BigInt(i) * 123_456_789n;
  secrets.push({ key: hex(key), counter: counter.toString(), micros: micros.toString(), out: hex(deriveSecret(key, counter, micros)) });
}
const uuids = [];
for (let i = 0; i < 6; i++) {
  const ms = 1_760_000_000_000n + BigInt(i) * 86_400_000n;
  const r = bytes(32);
  uuids.push({ ms: ms.toString(), rand: hex(r), out: uuidV7(ms, r) });
}
const pkce = ['verifier', 'dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk', base64UrlEncode(bytes(32)), 'ü'].map((v) => ({ verifier: v, challenge: pkceChallenge(v) }));

const redirects = ['/dashboard?tab=billing', '/', '', 'https://attacker.example', '//attacker.example', '/%2f%2fattacker.example', '/\\attacker.example', '/%5cattacker.example', '/ok%0d%0alocation:%20https://attacker.example', '/ok\u007fblocked', '/path#fragment', '/%23frag', '/%E0%A4%A', '/caf%C3%A9', '/' + 'a'.repeat(2047), '/' + 'a'.repeat(2048), 'relative', '/a/../b', '/%zz'].map((v) => ({ value: v, result: safeRedirectPath(v) ?? null }));
const queries = ['/auth/x?code=abc&state=d%2Bef', '/x', '/x?a', '/x?a=1&a=2', '/x?redirectTo=%2Fhome%3Ftab%3D1', '/x?bad=%ZZ&good=1', '/x?plus=a+b', '/x?=v&k=', '/x?k=%F0%9F%9A%80'].map((uri) => ({ uri, result: parseQueryString(uri) }));

writeFileSync(new URL('../tests/vectors/auth.json', import.meta.url), JSON.stringify({ commit: '37ee2ad627568f95df8229647cff5a482d629c25', dummyPasswordHash: DUMMY_PASSWORD_HASH, keys, external, jwts, verify, scrypt, verifyPassword: verifyPw, secrets, uuids, pkce, redirects, queries }, null, 1) + '\n');
console.log(`keys ${keys.length}, external ${external.length}, jwts ${jwts.length}, verify ${verify.length}, scrypt ${scrypt.length}, verifyPassword ${verifyPw.length}, secrets ${secrets.length}, uuids ${uuids.length}, pkce ${pkce.length}, redirects ${redirects.length}, queries ${queries.length}`);
