// @spacetimedb/files port: the upstream test cases (scripts/test.ts at the pinned commit:
// SHA-256 ETags, query parsing, owner/path keys, path and media-type validation, response
// headers) plus every helper end to end and the HTTP serve route (GET/HEAD, ETag/304, access,
// inline vs attachment).
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { HOST, DB, call, callErr, procErr, sql, newIdentity, test, done, unwrap, some, none } from './lib.mjs';

const BASE = `${HOST}/v1/database/${DB}/route`;
const opt = (v) => (v === undefined || v === null ? none : some(v));
const fromOpt = (o) => (o[0] === 0 ? o[1] : undefined);
const hex = (s) => Buffer.from(s).toString('hex');
const sha = (b) => createHash('sha256').update(b).digest('hex');
const summary = ([id, path, mimeType, size, sha256Hex, visibility]) => ({ id, path, mimeType, size, sha256Hex, visibility });
const page = ([files, next]) => ({ files: files.map(summary), next: fromOpt(next) });
const uploadAs = (owner, path, body, { mime = 'text/plain', visibility = 'owner', max } = {}) =>
  call('demo_upload_as', [owner, path, mime, hex(body), visibility, opt(max)]);
const uploadErr = (owner, path, body, o = {}) =>
  procErr('demo_upload_as', [owner, path, o.mime ?? 'text/plain', typeof body === 'string' ? hex(body) : body, o.visibility ?? 'owner', opt(o.max)]);
const listAs = async (owner, prefix, cursor, limit) => page(unwrap(await call('demo_list_as', [owner, prefix, opt(cursor), opt(limit)])));
const get = (path, headers = {}, method = 'GET') => fetch(`${BASE}${path}`, { method, headers });

await test('upstream: SHA-256 ETags, query parsing, owner/path keys', async () => {
  assert.equal(unwrap(await call('demo_hash_hex', [hex('abc')])), 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad');
  assert.equal(unwrap(await call('demo_hash_hex', [''])), 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855');
  const q = async (uri, name) => fromOpt(unwrap(await call('demo_query_param', [uri, name])));
  assert.equal(await q('/files?id=42', 'id'), '42');
  assert.equal(await q('/files?name=hello+world', 'name'), 'hello world');
  assert.equal(await q('/files?id=%ZZ', 'id'), undefined);
  assert.equal(await q('/files?other=1', 'id'), undefined);
  assert.equal(await q('/files?bad=%ZZ&id=1', 'id'), '1', 'values of other keys are never decoded');
  assert.equal(await q('/files?%ZZ=1&id=1', 'id'), undefined, 'a malformed key before the match aborts, as in TS');
  assert.equal(await q('/files?id=1&bad=%ZZ', 'id'), '1');
  assert.equal(await q('/files?&id=%F0%9F%9A%80', 'id'), '🚀');
  const key = async (o, p) => unwrap(await call('demo_owner_path_key', [o, p]));
  assert.notEqual(await key('owner-a', '/avatar.png'), await key('owner-b', '/avatar.png'));
  assert.notEqual(await key('a:b', '/c'), await key('a', '/b/c'));
  assert.equal(await key('é😀', '/x'), '3:é😀/x', 'UTF-16 length, as in TS');
});

await test('upstream: path and media-type validation', async () => {
  const path = async (p) => procErr('demo_validate_path', [p]);
  assert.equal(await path('/docs/readme.txt'), null);
  for (const bad of ['docs/readme.txt', '/docs/../secret', '/docs/blocked\u007f.txt', '/', '/a/', '/a//b', '/a\\b', '/./x', `/${'s'.repeat(256)}`, `/${'p'.repeat(1024)}`]) {
    assert.equal(await path(bad), 'files.invalid_path', JSON.stringify(bad).slice(0, 40));
  }
  assert.equal(await path(`/${'s'.repeat(255)}`), null);
  const mime = async (m) => call('demo_validate_mime', [m]);
  assert.equal(unwrap(await mime('Image/SVG+XML')), 'image/svg+xml');
  assert.equal(unwrap(await mime('  text/plain ')), 'text/plain');
  for (const bad of ['text/plain\r\nx-injected: yes', 'text/plain; charset=utf-8', 'text', 'a/b/c', '/plain', `a/${'b'.repeat(126)}`]) {
    assert.equal(await procErr('demo_validate_mime', [bad]), 'files.invalid_mime_type', bad);
  }
});

await test('upload: validation order, size cap, owner quota (replacing a file does not double count)', async () => {
  assert.equal(await uploadErr('', '/a.txt', 'x'), 'files.invalid_owner');
  assert.equal(await uploadErr('o'.repeat(513), '/a.txt', 'x'), 'files.invalid_owner');
  assert.equal(await uploadErr('bad\u0001owner', '/a.txt', 'x'), 'files.invalid_owner');
  assert.equal(await uploadErr('q1', 'a.txt', 'x'), 'files.invalid_path');
  assert.equal(await uploadErr('q1', '/a.txt', 'x', { mime: 'nope' }), 'files.invalid_mime_type');
  assert.equal(await uploadErr('q1', '/a.txt', 'x', { visibility: 'everyone' }), 'files.invalid_visibility');
  // Generated in the module: a 4 MB JSON argument exceeds the HTTP API's request limit.
  assert.equal(await procErr('demo_upload_zeros', ['q1', '/big.bin', 4_000_001]), 'files.too_large:4000001/4000000');
  assert.equal(await procErr('demo_upload_zeros', ['q1', '/big.bin', 4_000_000]), null, 'exactly the limit is fine');
  await call('demo_delete_as', ['q1', '/big.bin']);
  await uploadAs('q1', '/one.txt', 'abcdef', { max: 10 });
  assert.equal(await uploadErr('q1', '/two.txt', 'abcdef', { max: 10 }), 'files.quota_exceeded');
  await uploadAs('q1', '/one.txt', 'abcdefghij', { max: 10 });
  const [row] = await sql(`SELECT * FROM file WHERE owner_path_key = '${'2:q1/one.txt'}'`);
  assert.equal(row.size, 10);
  assert.equal(row.sha_256_hex, sha('abcdefghij'));
  const [blob] = await sql(`SELECT * FROM file_blob WHERE file_id = ${row.id}`);
  assert.ok(blob, 'blob stored apart from metadata');
  // Different owners may use the same path.
  await uploadAs('q2', '/one.txt', 'zz', { max: 10 });
  assert.equal((await sql(`SELECT * FROM file WHERE path = '/one.txt'`)).length, 2);
});

await test('list: ordered by path, prefix ranges, cursor paging, and listing errors', async () => {
  for (const p of ['/a/3', '/a/1', '/a/2', '/a/5', '/a/4', '/ab', '/b/x', '/a/é', '/a/zz/deep']) await uploadAs('lister', p, p);
  let r = await listAs('lister', '/a/', undefined, 2);
  assert.deepEqual(r.files.map((f) => f.path), ['/a/1', '/a/2']);
  assert.equal(r.next, '/a/2');
  r = await listAs('lister', '/a/', r.next, 3);
  assert.deepEqual(r.files.map((f) => f.path), ['/a/3', '/a/4', '/a/5']);
  r = await listAs('lister', '/a/', r.next, 3);
  assert.deepEqual(r.files.map((f) => f.path), ['/a/zz/deep', '/a/é']);
  assert.equal(r.next, undefined);
  r = await listAs('lister', '');
  assert.deepEqual(r.files.map((f) => f.path), ['/a/1', '/a/2', '/a/3', '/a/4', '/a/5', '/a/zz/deep', '/a/é', '/ab', '/b/x']);
  assert.equal(r.files[0].sha256Hex, sha('/a/1'));
  assert.equal(r.files[0].mimeType, 'text/plain');
  assert.equal((await listAs('lister', '/a')).files.length, 8, "'/a' also matches '/ab'");
  assert.equal(await procErr('demo_list_as', ['lister', 'a/', none, none]), 'files.invalid_prefix');
  assert.equal(await procErr('demo_list_as', ['lister', '/a/', some('/b/x'), none]), 'files.invalid_cursor');
  assert.equal(await procErr('demo_list_as', ['lister', '/a/', some('not-a-path'), none]), 'files.invalid_path');
  assert.equal(await procErr('demo_list_as', ['lister', '/a/', none, some(0)]), 'files.invalid_page_size');
  assert.equal(await procErr('demo_list_as', ['lister', '/a/', none, some(201)]), 'files.invalid_page_size');
  assert.equal((await listAs('lister', '/a/', undefined, 200)).files.length, 7);
});

await test('caller operations: upload, view, read, rename, visibility, delete (owner = the caller)', async () => {
  const u = await newIdentity();
  const other = await newIdentity();
  await call('upload_file', ['/docs/note.txt', 'Text/Plain', hex('hello'), 'owner'], u);
  await call('upload_file', ['/docs/raw.bin', '', hex('\x00\x01'), 'owner'], u);
  const view = await sql('SELECT * FROM my_file_summaries', u);
  assert.deepEqual(view.map((f) => f.path), ['/docs/note.txt', '/docs/raw.bin']);
  assert.equal(view[1].mime_type, 'application/octet-stream');
  assert.equal(view[0].owner_user_id, undefined, 'summaries omit the owner');
  assert.equal((await sql('SELECT * FROM my_file_summaries', other)).length, 0);
  const [bytes, mime] = unwrap(await call('read_file_bytes', ['/docs/note.txt'], u));
  assert.equal(Buffer.from(bytes.replace(/^0x/, ''), 'hex').toString(), 'hello');
  assert.equal(mime, 'text/plain');
  assert.equal(await procErr('read_file_bytes', ['/docs/note.txt'], other), 'files.not_found');
  assert.equal(await callErr('rename_file', ['/docs/note.txt', '/docs/raw.bin'], u), 'files.path_taken');
  assert.equal(await callErr('rename_file', ['/nope.txt', '/x.txt'], u), 'files.not_found');
  await call('rename_file', ['/docs/note.txt', '/docs/renamed.txt'], u);
  await call('rename_file', ['/docs/renamed.txt', '/docs/renamed.txt'], u);
  assert.deepEqual(page(unwrap(await call('list_files', ['/docs/', none, none], u))).files.map((f) => f.path), ['/docs/raw.bin', '/docs/renamed.txt']);
  assert.equal(await callErr('set_file_visibility', ['/docs/raw.bin', 'world'], u), 'files.invalid_visibility');
  await call('set_file_visibility', ['/docs/raw.bin', 'public'], u);
  await call('delete_file', ['/docs/raw.bin'], u);
  await call('delete_file', ['/docs/raw.bin'], u);
  assert.equal(await callErr('delete_file', ['bad'], u), 'files.invalid_path');
  assert.deepEqual((await sql('SELECT * FROM my_file_summaries', u)).map((f) => f.path), ['/docs/renamed.txt']);
});

await test('HTTP: public files served with ETag, cache and safety headers; HEAD; 304; inline images only', async () => {
  const png = unwrap(await uploadAs('web', '/img.png', 'PNGDATA', { mime: 'image/png', visibility: 'public' }));
  const html = unwrap(await uploadAs('web', '/page.html', '<script>x</script>', { mime: 'text/html', visibility: 'public' }));
  let r = await get(`/files?id=${png}`);
  assert.equal(r.status, 200);
  assert.equal(await r.text(), 'PNGDATA');
  assert.equal(r.headers.get('content-type'), 'image/png');
  assert.equal(r.headers.get('etag'), `"${sha('PNGDATA')}"`);
  assert.equal(r.headers.get('cache-control'), 'public, max-age=300, must-revalidate');
  assert.equal(r.headers.get('x-content-type-options'), 'nosniff');
  assert.equal(r.headers.get('content-security-policy'), 'sandbox');
  assert.equal(r.headers.get('content-disposition'), null);
  r = await get(`/files?id=${html}`);
  assert.equal(r.headers.get('content-disposition'), 'attachment');
  r = await get(`/files?id=${png}`, { 'if-none-match': `"${sha('PNGDATA')}"` });
  assert.equal(r.status, 304);
  assert.equal(r.headers.get('etag'), `"${sha('PNGDATA')}"`);
  assert.equal(await r.text(), '');
  r = await get(`/files?id=${png}`, { 'if-none-match': '"other"' });
  assert.equal(r.status, 200);
  r = await get(`/files?id=${png}`, {}, 'HEAD');
  assert.equal(r.status, 200);
  assert.equal(r.headers.get('etag'), `"${sha('PNGDATA')}"`);
  r = await get(`/files?id=${encodeURIComponent(` 0x${png.toString(16)} `)}`);
  assert.equal(r.status, 200, 'ids parse like JS BigInt (whitespace, 0x)');
});

await test('HTTP: private files, access check, bad requests', async () => {
  const priv = unwrap(await uploadAs('web', '/secret.txt', 'top secret', { visibility: 'owner' }));
  let r = await get(`/files?id=${priv}`);
  assert.equal(r.status, 403);
  assert.equal(await r.text(), 'forbidden');
  r = await get(`/files-any?id=${priv}`);
  assert.equal(r.status, 200, 'canAccess admits it');
  assert.equal(await r.text(), 'top secret');
  assert.equal(r.headers.get('cache-control'), 'private, max-age=60, must-revalidate');
  for (const [q, status, body] of [['', 400, 'missing id'], ['?id=', 400, 'missing id'], ['?id=abc', 400, 'bad id'], ['?id=0', 400, 'bad id'], ['?id=-3', 400, 'bad id'], ['?id=1.5', 400, 'bad id'], ['?id=999999', 404, 'not found'], ['?id=%ZZ', 400, 'missing id']]) {
    r = await get(`/files${q}`);
    assert.equal(r.status, status, q);
    assert.equal(await r.text(), body, q);
  }
  r = await fetch(`${BASE}/files?id=${priv}`, { method: 'POST' });
  assert.equal(r.status, 405);
  assert.equal(await r.text(), 'method not allowed');
});

done();
