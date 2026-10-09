// resend: the upstream unit, webhook and smoke-test cases, against a demo module built with
// RESEND_API_BASE=http://127.0.0.1:4201 and a mock Resend API on that port.
import assert from 'node:assert/strict';
import { createHmac } from 'node:crypto';
import { call, callErr, done, HOST, DB, newIdentity, procErr, sql, test, unwrap } from './lib.mjs';
import { startMock } from './mock-http.mjs';

const PORT = Number(process.env.RESEND_MOCK_PORT ?? 4201);
const none = [1, []];
const some = (v) => [0, v];
const opt = (v) => (v === undefined ? { none: [] } : { some: v });
const STATUS = ['Queued', 'Sent', 'Delivered', 'DeliveryDelayed', 'Bounced', 'Failed', 'Cancelled'];
const EVENT_STATUS = ['Received', 'Processed', 'Ignored', 'Failed'];
const unopt = (o) => (o[0] === 0 ? o[1] : undefined);

// --- the mock Resend API ---------------------------------------------------------------
let nextId = 1;
let sendReply = null; // override the next POST /emails reply
const mock = await startMock(PORT, (r) => {
  if (r.method === 'POST' && r.path === '/emails') {
    if (sendReply) {
      const reply = sendReply;
      sendReply = null;
      return reply;
    }
    return { status: 200, json: { id: `em_mock_${nextId++}` } };
  }
  const cancel = r.path.match(/^\/emails\/([^/]+)\/cancel$/);
  if (r.method === 'POST' && cancel) {
    return cancel[1] === 'em_unknown'
      ? { status: 404, json: { name: 'not_found', message: 'Email   not\nfound', statusCode: 404 } }
      : { status: 200, json: { object: 'email', id: cancel[1] } };
  }
  if (r.method === 'GET' && r.path === '/domains') return { status: 200, json: { data: [] } };
  if (r.method === 'DELETE' && r.path === '/domains/d_1') return { status: 200, json: { deleted: true } };
  return { status: 404, json: { name: 'not_found', message: 'no route' } };
});

// --- helpers ---------------------------------------------------------------------------
const SECRET_RAW = 'resend_smoke_test_secret';
const SECRET = `whsec_${Buffer.from(SECRET_RAW).toString('base64')}`;
const svix = (id, ts, body) => `v1,${createHmac('sha256', SECRET_RAW).update(`${id}.${ts}.${body}`).digest('base64')}`;
const now = () => String(Math.floor(Date.now() / 1000));
let n = 0;
const uniq = (p) => `${p}_${Date.now()}_${n++}`;

function payload({ type, emailId, to, extra, createdAt, subject, tags }) {
  return JSON.stringify({
    type,
    created_at: createdAt ?? '2026-05-04T00:00:00Z',
    data: {
      email_id: emailId,
      created_at: '2026-05-04T00:00:00Z',
      from: 'onboarding@resend.dev',
      to: to ?? ['delivered@resend.dev'],
      subject: subject ?? 'smoke test',
      ...(tags ? { tags } : {}),
      ...(extra ?? {}),
    },
  });
}

async function ingest(eventId, type, body, who) {
  const ts = now();
  return call('ingest_resend_webhook', [eventId, type, body, opt(svix(eventId, ts, body)), opt(ts)], who);
}
async function postWebhook(eventId, body, { sig, ts = now(), method = 'POST' } = {}) {
  const r = await fetch(`${HOST}/v1/database/${DB}/route/webhook/resend`, {
    method,
    headers: { 'content-type': 'application/json', 'svix-id': eventId, 'svix-timestamp': ts, 'svix-signature': sig ?? svix(eventId, ts, body) },
    body: method === 'POST' ? body : undefined,
  });
  return { status: r.status, json: await r.json().catch(() => null) };
}
async function email(id) {
  const [row] = await sql(`SELECT * FROM resend_email WHERE resend_id = '${id}'`);
  if (!row) return undefined;
  return { ...row, status: STATUS[row.status[0]], last_error: unopt(row.last_error), html: unopt(row.html), user_id: unopt(row.user_id) };
}
async function webhookStatus(id) {
  const [row] = await sql(`SELECT * FROM resend_webhook_event WHERE event_id = '${id}'`);
  return row && EVENT_STATUS[row.status[0]];
}
const sendArgs = (o = {}) => [
  opt(o.from), o.to ?? ['delivered@resend.dev'], o.subject ?? 'Welcome', opt(o.html), opt(o.text ?? (o.html ? undefined : 'Hello')),
  opt(o.cc), opt(o.bcc), opt(o.replyTo), opt(o.tagsJson), opt(o.headersJson), opt(o.scheduledAt), opt(o.idempotencyKey),
];

// --- before config -----------------------------------------------------------------------
await test('send before config: resend.config_not_set; webhook route answers 500 not configured', async () => {
  assert.match(await procErr('resend_send_email', sendArgs()), /resend\.config_not_set/);
  const r = await postWebhook(uniq('evt'), payload({ type: 'email.sent', emailId: 'em_x' }));
  assert.equal(r.status, 500);
  assert.equal(r.json.code, 'resend.webhook_secret_not_configured');
});

const stranger = await newIdentity();

await test('admin gate: strangers can not configure, send or query', async () => {
  assert.equal(await procErr('set_resend_config', ['re_x', opt(), opt()], stranger), 'resend.not_authorized');
  assert.equal(await procErr('get_resend_config_status', [], stranger), 'resend.not_authorized');
  assert.equal(await procErr('resend_send_email', sendArgs(), stranger), 'resend.not_authorized');
  assert.equal(await procErr('resend_get_email', ['em_a'], stranger), 'resend.not_authorized');
  assert.equal(await procErr('resend_add_admin_identity', [{ __identity__: '0x' + stranger.identity }], stranger), 'resend.not_authorized');
  assert.match(await callErr('resend_replay_webhook_event', ['x'], stranger), /resend\.not_authorized/);
});

await test('set_resend_config / get_resend_config_status (None keeps stored values)', async () => {
  unwrap(await call('set_resend_config', ['re_smoke_placeholder', opt(SECRET), opt('onboarding@resend.dev')]));
  let s = unwrap(await call('get_resend_config_status', []));
  assert.deepEqual(s, [true, true, some('onboarding@resend.dev'), 20]);
  unwrap(await call('set_resend_config', ['re_k2', opt(), opt()]));
  s = unwrap(await call('get_resend_config_status', []));
  assert.deepEqual(s, [true, true, some('onboarding@resend.dev'), 5]);
  unwrap(await call('set_resend_config', ['re_smoke_placeholder', opt(), opt()]));
});

await test('admins: add, remove, and never remove the last one', async () => {
  const other = await newIdentity();
  unwrap(await call('resend_add_admin_identity', [{ __identity__: '0x' + other.identity }]));
  assert.deepEqual(unwrap(await call('get_resend_config_status', [], other))[0], true);
  unwrap(await call('resend_remove_admin_identity', [{ __identity__: '0x' + other.identity }], other));
  assert.equal(await procErr('get_resend_config_status', [], other), 'resend.not_authorized');
  const [me] = await sql('SELECT * FROM resend_admin_identity');
  assert.equal(await procErr('resend_remove_admin_identity', [me.identity]), 'resend.cannot_remove_last_admin');
});

// --- input validation (upstream test-unit cases plus the documented limits) ---------------
await test('email input validation: the upstream unit cases and limits', async () => {
  const cases = [
    [{ to: [] }, 'resend.send_email_no_recipients'],
    [{ to: ['user@example.com\u007fblocked'] }, 'resend.to_invalid_address'],
    [{ subject: 'Welcome\r\nx-injected: yes' }, 'resend.send_email_invalid_subject'],
    [{ to: Array.from({ length: 101 }, (_, i) => `user${i}@example.com`) }, 'resend.to_too_many'],
    [{ to: ['a@x.co'], cc: Array.from({ length: 60 }, (_, i) => `c${i}@x.co`), bcc: Array.from({ length: 40 }, (_, i) => `b${i}@x.co`) }, 'resend.send_email_too_many_recipients'],
    [{ cc: [''] }, 'resend.cc_invalid_address'],
    [{ bcc: ['x'.repeat(321)] }, 'resend.bcc_invalid_address'],
    [{ replyTo: Array.from({ length: 101 }, () => 'r@x.co') }, 'resend.reply_to_too_many'],
    [{ from: '' }, 'resend.send_email_invalid_from'],
    [{ subject: '' }, 'resend.send_email_invalid_subject'],
    [{ subject: 's'.repeat(999) }, 'resend.send_email_invalid_subject'],
    [{ html: 'h'.repeat(200_001) }, 'resend.send_email_html_too_large'],
    [{ tagsJson: '{bad' }, 'resend.tags_invalid_json'],
    [{ tagsJson: '"string"' }, 'resend.tags_invalid_json'],
    [{ headersJson: '[' + '1,'.repeat(9000) + '1]' }, 'resend.headers_too_large'],
    [{ scheduledAt: 'in 1 hour\n' }, 'resend.send_email_invalid_schedule'],
    [{ idempotencyKey: 'k'.repeat(257) }, 'resend.idempotency_key_too_long'],
  ];
  for (const [o, code] of cases) assert.equal(await procErr('resend_send_email', sendArgs(o)), code, JSON.stringify(o).slice(0, 80));
  // missing content: both html and text absent
  const args = sendArgs();
  args[3] = opt();
  args[4] = opt();
  assert.equal(await procErr('resend_send_email', args), 'resend.send_email_missing_content');
});

await test('resend_api_request: method and path checks (upstream unit cases), then a real call', async () => {
  const req = (m, p, body, key) => call('resend_api_request', [m, p, opt(body), opt(key)]);
  assert.equal(await procErr('resend_api_request', ['TRACE', '/emails', opt(), opt()]), 'resend.request_method_invalid');
  assert.equal(await procErr('resend_api_request', ['GET', 'https://attacker.example/collect', opt(), opt()]), 'resend.request_path_invalid');
  assert.equal(await procErr('resend_api_request', ['GET', '//attacker.example', opt(), opt()]), 'resend.request_path_invalid');
  assert.equal(await procErr('resend_api_request', ['GET', '/emails\u007fblocked', opt(), opt()]), 'resend.request_path_invalid');
  assert.equal(await procErr('resend_api_request', ['GET', '/emails#frag', opt(), opt()]), 'resend.request_path_invalid');
  assert.equal(await procErr('resend_api_request', ['POST', '/emails', opt('x'.repeat(256 * 1024 + 1)), opt()]), 'resend.request_body_too_large');
  mock.clear();
  const res = unwrap(await req(' get ', '  /domains ', undefined, undefined));
  assert.deepEqual(res, [200, '{"data":[]}']);
  const r = mock.last();
  assert.equal(r.method, 'GET');
  assert.equal(r.path, '/domains');
  assert.equal(r.headers.authorization, 'Bearer re_smoke_placeholder');
  assert.equal(r.headers.accept, 'application/json');
  assert.equal(r.headers['content-type'], undefined);
  assert.deepEqual(unwrap(await req('DELETE', '/domains/d_1', undefined, 'idem-1')), [200, '{"deleted":true}']);
  assert.equal(mock.last().headers['idempotency-key'], 'idem-1');
});

// --- sending -------------------------------------------------------------------------------
await test('send: request shape, default sender, queued row with tag fields', async () => {
  mock.clear();
  const tags = JSON.stringify([{ name: 'userId', value: 'u_123' }, { name: 'orgId', value: 'launch' }]);
  const res = unwrap(await call('resend_send_email', sendArgs({
    to: ['delivered@resend.dev'], html: '<p>Hello.</p>', cc: [], bcc: ['b@x.co'], replyTo: ['r@x.co'],
    tagsJson: tags, headersJson: '{"X-Entity-Ref-ID":"123"}', scheduledAt: 'in 1 min', idempotencyKey: 'welcome-1',
  })));
  const id = res[0];
  assert.match(id, /^em_mock_/);
  const r = mock.last();
  assert.equal(r.method, 'POST');
  assert.equal(r.path, '/emails');
  assert.equal(r.headers['content-type'], 'application/json');
  assert.equal(r.headers['idempotency-key'], 'welcome-1');
  assert.equal(
    r.body,
    JSON.stringify({
      from: 'onboarding@resend.dev', to: ['delivered@resend.dev'], subject: 'Welcome', html: '<p>Hello.</p>',
      bcc: ['b@x.co'], reply_to: ['r@x.co'], scheduled_at: 'in 1 min',
      tags: [{ name: 'userId', value: 'u_123' }, { name: 'orgId', value: 'launch' }], headers: { 'X-Entity-Ref-ID': '123' },
    })
  );
  const row = await email(id);
  assert.equal(row.status, 'Queued');
  assert.equal(row.user_id, 'u_123');
  assert.equal(unopt(row.org_id), 'launch');
  assert.equal(row.to_addresses_json, '["delivered@resend.dev"]');
  const listed = unwrap(await call('resend_list_emails_by_user_id', ['u_123']));
  assert.ok(listed.some((e) => e[0] === id));
  assert.ok(unwrap(await call('resend_list_emails_by_org_id', ['launch'])).some((e) => e[0] === id));
  assert.ok(unwrap(await call('resend_list_emails_by_status', [{ queued: [] }])).some((e) => e[0] === id));
});

await test('send body is serialized like JS JSON.stringify (key order, numbers, non-ASCII)', async () => {
  mock.clear();
  const headersJson = '{"b":1,"10":2.0,"a":[1.5,"x"],"2":{"z":1,"0":true}}';
  unwrap(await call('resend_send_email', sendArgs({ html: '<p>héllo 🎉 "q"</p>', headersJson })));
  assert.equal(
    mock.last().body,
    JSON.stringify({ from: 'onboarding@resend.dev', to: ['delivered@resend.dev'], subject: 'Welcome', html: '<p>héllo 🎉 "q"</p>', headers: JSON.parse(headersJson) })
  );
});

await test('provider errors: status, name and collapsed message; invalid responses', async () => {
  sendReply = { status: 422, json: { name: 'validation_error', message: 'Bad\n   thing', statusCode: 422 } };
  assert.equal(await procErr('resend_send_email', sendArgs()), 'resend.send_email_failed:422:name=validation_error|msg=Bad thing');
  sendReply = { status: 500, body: '  upstream\n\n exploded  ' };
  assert.equal(await procErr('resend_send_email', sendArgs()), 'resend.send_email_failed:500:body=upstream exploded');
  sendReply = { status: 503, body: '' };
  assert.equal(await procErr('resend_send_email', sendArgs()), 'resend.send_email_failed:503');
  sendReply = { status: 200, body: 'not json' };
  assert.equal(await procErr('resend_send_email', sendArgs()), 'resend.send_email_invalid_response');
  sendReply = { status: 200, json: { object: 'email' } };
  assert.match(await procErr('resend_send_email', sendArgs()), /^resend\.send_email_invalid_response:/);
});

await test('cancel: calls /emails/:id/cancel and marks the row Cancelled; provider failure reported', async () => {
  const id = unwrap(await call('resend_send_email', sendArgs({ scheduledAt: 'tomorrow' })))[0];
  mock.clear();
  unwrap(await call('resend_cancel_email', [id]));
  assert.equal(mock.last().path, `/emails/${id}/cancel`);
  assert.equal((await email(id)).status, 'Cancelled');
  assert.equal(await procErr('resend_cancel_email', ['em_unknown']), 'resend.cancel_email_failed:404:name=not_found|msg=Email not found');
});

// --- webhooks (the upstream smoke test, plus HTTP-route cases) --------------------------------
await test('HTTP route: wrong signature 401 with the crypto reason, nothing stored', async () => {
  const id = uniq('evt_bad_http');
  const body = payload({ type: 'email.sent', emailId: 'em_bad' });
  const r = await postWebhook(id, body, { sig: svix('evt_other', now(), body) });
  assert.equal(r.status, 401);
  assert.equal(r.json.code, 'resend.webhook_signature_mismatch:crypto.signature_mismatch');
  assert.equal(await webhookStatus(id), undefined);
  const old = await postWebhook(uniq('evt_old'), body, { ts: String(Math.floor(Date.now() / 1000) - 3600) });
  assert.equal(old.json.code, 'resend.webhook_signature_mismatch:crypto.timestamp_outside_tolerance');
  const get = await fetch(`${HOST}/v1/database/${DB}/route/webhook/resend`);
  assert.equal(get.status, 404, 'only POST is routed');
});

await test('HTTP route: invalid signed payload is 400 on first delivery and redelivery, row Failed', async () => {
  const id = uniq('evt_invalid_http');
  const body = JSON.stringify({ type: 'email.sent', data: {} });
  for (let attempt = 0; attempt < 2; attempt++) {
    const r = await postWebhook(id, body);
    assert.equal(r.status, 400);
    assert.equal(r.json.code, 'resend.webhook_payload_invalid');
  }
  assert.equal(await webhookStatus(id), 'Failed');
  const missingType = await postWebhook(uniq('evt_no_type'), JSON.stringify({ data: {} }));
  assert.equal(missingType.status, 400);
  assert.equal(missingType.json.code, 'resend.webhook_payload_invalid');
});

await test('HTTP route: a handled event is applied and acknowledged', async () => {
  const id = uniq('evt_http_ok');
  const em = uniq('em_http');
  const r = await postWebhook(id, payload({ type: 'email.delivered', emailId: em }));
  assert.deepEqual([r.status, r.json], [200, { ok: true, code: 'ok' }]);
  assert.equal((await email(em)).status, 'Delivered');
});

await test('reducer: signed type must match the supplied event type', async () => {
  const id = uniq('evt_mismatch');
  const body = payload({ type: 'email.delivered', emailId: 'em_m' });
  const ts = now();
  assert.match(await callErr('ingest_resend_webhook', [id, 'email.bounced', body, opt(svix(id, ts, body)), opt(ts)]), /resend\.webhook_metadata_mismatch/);
  assert.match(await callErr('ingest_resend_webhook', ['', 'email.delivered', body, opt(svix('', ts, body)), opt(ts)]), /resend\.webhook_metadata_invalid/);
  assert.match(await callErr('ingest_resend_webhook', [id, 'email.delivered', body, opt(svix(id, ts, body)), opt('x'.repeat(8193))]), /resend\.webhook_header_too_large/);
});

await test('reducer keeps a Failed row for an invalid signed payload; replay keeps it Failed', async () => {
  const id = uniq('evt_invalid_reducer');
  await ingest(id, 'email.sent', JSON.stringify({ type: 'email.sent', data: {} }));
  assert.equal(await webhookStatus(id), 'Failed');
  await call('resend_replay_webhook_event', [id]);
  assert.equal(await webhookStatus(id), 'Failed');
  const [row] = await sql(`SELECT * FROM resend_webhook_event WHERE event_id = '${id}'`);
  assert.match(unopt(row.error_message), /^payload validation failed: /);
  const badTime = uniq('evt_bad_time');
  await ingest(badTime, 'email.sent', payload({ type: 'email.sent', emailId: 'em_t', createdAt: 'yesterday' }));
  assert.equal(await webhookStatus(badTime), 'Failed');
});

await test('signed event types the module does not handle are stored as Ignored', async () => {
  const id = uniq('evt_contact');
  await ingest(id, 'contact.created', JSON.stringify({ type: 'contact.created', created_at: '2026-05-04T00:00:00Z', data: { id: 'c1' } }));
  assert.equal(await webhookStatus(id), 'Ignored');
  const r = await postWebhook(uniq('evt_domain'), JSON.stringify({ type: 'domain.updated', data: {} }));
  assert.equal(r.status, 200);
});

const A = uniq('em_a');
await test('happy path: sent → delivered; complained, opened, clicked are flags only', async () => {
  await ingest(uniq('evt'), 'email.sent', payload({ type: 'email.sent', emailId: A, tags: { userId: 'u_9', orgId: 'o_9' } }));
  assert.equal((await email(A)).status, 'Sent');
  await ingest(`${A}_b`, 'email.delivered', payload({ type: 'email.delivered', emailId: A }));
  assert.equal((await email(A)).status, 'Delivered');
  await ingest(uniq('evt'), 'email.complained', payload({ type: 'email.complained', emailId: A }));
  await ingest(uniq('evt'), 'email.opened', payload({ type: 'email.opened', emailId: A }));
  const click = { ipAddress: '203.0.113.42', link: 'https://spacetimedb.com', timestamp: '2026-05-04T00:00:00Z', userAgent: 'Mozilla/5.0 (smoke-test)' };
  await ingest(uniq('evt'), 'email.clicked', payload({ type: 'email.clicked', emailId: A, extra: { click: { ...click, extra: 'dropped' } } }));
  const row = await email(A);
  assert.equal(row.status, 'Delivered');
  assert.equal(row.complained, true);
  assert.equal(row.opened, true);
  assert.equal(row.clicked, true);
  assert.equal(row.user_id, 'u_9');
  assert.equal(unopt(row.tags_json), '{"userId":"u_9","orgId":"o_9"}');
  const events = unwrap(await call('resend_list_delivery_events_for_email', [A]));
  const clicked = events.find((e) => e[2] === 'email.clicked');
  assert.equal(unopt(clicked[4]), JSON.stringify(click), 'click detail keeps only the schema keys, in order');
  assert.equal(events.length, 5);
});

const C = uniq('em_c');
await test('bounce with detail; late sent/delayed/failed and replay do not undo it', async () => {
  const bounce = { message: 'Mailbox does not exist', subType: 'NoEmail', type: 'Permanent' };
  await ingest(uniq('evt'), 'email.bounced', payload({ type: 'email.bounced', emailId: C, to: ['bounced@resend.dev'], extra: { bounce } }));
  let row = await email(C);
  assert.equal(row.status, 'Bounced');
  assert.equal(row.last_error, 'Mailbox does not exist');
  assert.equal(unopt(row.bounce_json), JSON.stringify(bounce));
  const late = `${C}_late_sent`;
  await ingest(late, 'email.sent', payload({ type: 'email.sent', emailId: C, createdAt: '2026-05-03T23:59:00Z' }));
  await ingest(uniq('evt'), 'email.delivery_delayed', payload({ type: 'email.delivery_delayed', emailId: C, createdAt: '2026-05-03T23:59:00Z' }));
  assert.equal((await email(C)).status, 'Bounced');
  await ingest(uniq('evt'), 'email.failed', payload({ type: 'email.failed', emailId: C, extra: { failed: { reason: 'late failure' } } }));
  row = await email(C);
  assert.equal(row.status, 'Bounced');
  assert.equal(row.last_error, 'Mailbox does not exist');
  await call('resend_replay_webhook_event', [late]);
  assert.equal((await email(C)).status, 'Bounced');
});

await test('delivery_delayed and failed statuses with reasons', async () => {
  const D = uniq('em_d');
  await ingest(uniq('evt'), 'email.delivery_delayed', payload({ type: 'email.delivery_delayed', emailId: D }));
  assert.equal((await email(D)).status, 'DeliveryDelayed');
  const F = uniq('em_f');
  await ingest(uniq('evt'), 'email.failed', payload({ type: 'email.failed', emailId: F, extra: { failed: { reason: 'rate limited by destination MTA' } } }));
  const row = await email(F);
  assert.equal(row.status, 'Failed');
  assert.equal(unopt(row.failure_reason), 'rate limited by destination MTA');
  assert.equal(row.last_error, 'rate limited by destination MTA');
});

await test('idempotency: re-ingesting a processed event is a no-op; replay keeps state; unknown replay fails', async () => {
  await ingest(`${A}_b`, 'email.delivered', payload({ type: 'email.delivered', emailId: A }));
  assert.equal((await email(A)).status, 'Delivered');
  await call('resend_replay_webhook_event', [`${A}_b`]);
  assert.equal((await email(A)).status, 'Delivered');
  assert.match(await callErr('resend_replay_webhook_event', ['evt_does_not_exist_xyz']), /resend\.webhook_event_not_found:evt_does_not_exist_xyz/);
});

await test('a queued email keeps its html when webhooks arrive; earlier stage events can not regress', async () => {
  const id = unwrap(await call('resend_send_email', sendArgs({ html: '<b>kept</b>' })))[0];
  await ingest(uniq('evt'), 'email.delivered', payload({ type: 'email.delivered', emailId: id }));
  await ingest(uniq('evt'), 'email.sent', payload({ type: 'email.sent', emailId: id, createdAt: '2026-05-05T00:00:00Z' }));
  const row = await email(id);
  assert.equal(row.status, 'Delivered', 'sent (lower stage) does not replace delivered, even if newer');
  assert.equal(row.html, '<b>kept</b>');
});

await test('host procedure: anyone can send themselves a welcome; history is caller-scoped', async () => {
  const alice = await newIdentity();
  const bob = await newIdentity();
  const id = unwrap(await call('send_welcome', ['  Alice@Example.com '], alice))[0];
  assert.equal(JSON.parse(mock.last().body).to[0], 'alice@example.com');
  assert.ok((await call('my_emails', [], alice)).some((e) => e[0] === id));
  assert.equal((await call('my_emails', [], bob)).length, 0);
  assert.equal(await procErr('send_welcome', ['not an email'], bob), 'dispatch.invalid_email');
});

await mock.close();
done();
