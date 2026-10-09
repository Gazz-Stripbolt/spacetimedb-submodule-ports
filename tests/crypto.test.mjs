// @spacetimedb/crypto port: the upstream test vectors (scripts/test-vectors.ts at the pinned
// commit), run against a demo module through procedures.
import assert from 'node:assert/strict';
import { call, unwrap, test, done, some, none } from './lib.mjs';

const hexOf = (s) => Buffer.from(s, 'utf8').toString('hex');
const sha = async (hex, repeat = 1) => unwrap(await call('crypto_sha_256', [hex, repeat]));
const hmac = async (keyHex, msgHex) => unwrap(await call('crypto_hmac_sha_256', [keyHex, msgHex]));
const b64 = async (s) => call('crypto_base_64_to_hex', [s]);

await test('SHA-256: empty, "abc", NIST 448-bit, 1,000,000 × "a"', async () => {
  assert.equal(await sha(''), 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855');
  assert.equal(await sha(hexOf('abc')), 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad');
  assert.equal(
    await sha(hexOf('abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq')),
    '248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1'
  );
  assert.equal(await sha('61', 1_000_000), 'cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0');
});

await test('HMAC-SHA256: RFC 4231 test cases 1, 2, 3, 4, 6, 7', async () => {
  assert.equal(await hmac('0b'.repeat(20), hexOf('Hi There')), 'b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7');
  assert.equal(await hmac(hexOf('Jefe'), hexOf('what do ya want for nothing?')), '5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843');
  assert.equal(await hmac('aa'.repeat(20), 'dd'.repeat(50)), '773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe');
  assert.equal(await hmac('0102030405060708090a0b0c0d0e0f10111213141516171819', 'cd'.repeat(50)), '82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b');
  assert.equal(await hmac('aa'.repeat(131), hexOf('Test Using Larger Than Block-Size Key - Hash Key First')), '60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54');
  assert.equal(
    await hmac(
      'aa'.repeat(131),
      hexOf('This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.')
    ),
    '9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2'
  );
});

await test('timingSafeEqual', async () => {
  const eq = async (a, b) => unwrap(await call('crypto_timing_safe_equal', [a, b]));
  assert.equal(await eq('010203', '010203'), true);
  assert.equal(await eq('010203', '010204'), false);
  assert.equal(await eq('010203', '01020304'), false);
  assert.equal(await eq('', ''), true);
});

await test('hex: either case in, lowercase out; odd length and bad chars rejected', async () => {
  assert.deepEqual(await call('crypto_hex_roundtrip', ['DeadBEEF']), [0, 'deadbeef']);
  assert.equal((await call('crypto_hex_roundtrip', ['abc']))[0], 1);
  assert.equal((await call('crypto_hex_roundtrip', ['zz']))[0], 1);
});

await test('base64: RFC 4648 vectors, unpadded input, and every malformed case', async () => {
  for (const [plain, enc] of [['', ''], ['f', 'Zg=='], ['fo', 'Zm8='], ['foo', 'Zm9v'], ['foob', 'Zm9vYg=='], ['fooba', 'Zm9vYmE='], ['foobar', 'Zm9vYmFy']]) {
    assert.deepEqual(await b64(enc), [0, hexOf(plain)], enc);
  }
  assert.deepEqual(await b64('Zg'), [0, hexOf('f')]);
  for (const bad of ['A', '=AAA', 'A=AA', 'AA=A', 'AAAA=', 'AA===', 'Zh==', 'Zm9=', 'Zg==\n', 'ĀAAA', 'AAĀA']) {
    assert.equal((await b64(bad))[0], 1, `should reject ${JSON.stringify(bad)}`);
  }
});

// Stripe: build the header like Stripe does, then check every outcome.
const stripeSecret = 'whsec_test_secret_1234567890';
const stripeBody = '{"id":"evt_test","type":"customer.created"}';
const stripeTs = '1700000000';
const stripeMac = await hmac(hexOf(stripeSecret), hexOf(`${stripeTs}.${stripeBody}`));
const stripe = (o = {}) =>
  call('crypto_verify_stripe', [
    o.body ?? stripeBody,
    o.header ?? `t=${stripeTs},v1=${stripeMac}`,
    o.secret ?? stripeSecret,
    o.tolerance ?? none,
    o.now ?? Number(stripeTs),
  ]);

await test('Stripe: valid, mutated body, wrong secret, missing v1, bad timestamp, too old, key rotation', async () => {
  assert.equal(await stripe(), 'ok');
  assert.equal(await stripe({ body: stripeBody + 'x' }), 'crypto.signature_mismatch');
  assert.equal(await stripe({ secret: 'whsec_wrong' }), 'crypto.signature_mismatch');
  assert.equal(await stripe({ header: `t=${stripeTs}` }), 'crypto.missing_signature');
  assert.equal(await stripe({ header: `t=soon,v1=${stripeMac}` }), 'crypto.invalid_timestamp');
  assert.equal(await stripe({ tolerance: some(300), now: Number(stripeTs) + 1000 }), 'crypto.timestamp_outside_tolerance');
  assert.equal(await stripe({ header: `t=${stripeTs},v1=deadbeef,v1=${stripeMac}` }), 'ok');
});

await test('GitHub: docs vector, round trip, missing prefix, wrong secret', async () => {
  const secret = "It's a Secret to Everybody";
  const body = 'Hello, World!';
  const mac = await hmac(hexOf(secret), hexOf(body));
  assert.equal(mac, '757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17');
  const gh = (header, s = secret) => call('crypto_verify_github', [body, header, s]);
  assert.equal(await gh(`sha256=${mac}`), 'ok');
  assert.equal(await gh(mac), 'crypto.missing_signature');
  assert.equal(await gh(`sha256=${mac}`, 'wrong'), 'crypto.signature_mismatch');
});

await test('Svix (Resend): valid, mutated body, bad secret, no v1, too old', async () => {
  const keyHex = '42'.repeat(32);
  const key64 = Buffer.from(keyHex, 'hex').toString('base64');
  const id = 'msg_test_123';
  const ts = '1700000000';
  const body = '{"event":"email.delivered"}';
  const sig = Buffer.from(await hmac(keyHex, hexOf(`${id}.${ts}.${body}`)), 'hex').toString('base64');
  const svix = (o = {}) =>
    call('crypto_verify_svix', [o.body ?? body, id, ts, o.sig ?? `v1,${sig}`, o.secret ?? `whsec_${key64}`, o.tolerance ?? none, o.now ?? Number(ts)]);
  assert.equal(await svix(), 'ok');
  assert.equal(await svix({ body: body + 'x' }), 'crypto.signature_mismatch');
  assert.equal(await svix({ secret: 'whsec_not base64' }), 'crypto.invalid_secret');
  assert.equal(await svix({ sig: 'v2,abc' }), 'crypto.missing_signature');
  assert.equal(await svix({ tolerance: some(300), now: Number(ts) + 301 }), 'crypto.timestamp_outside_tolerance');
});

done();
