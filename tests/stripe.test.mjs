// stripe: the upstream unit, webhook and smoke-test cases plus the billing procedures, against
// a demo module built with STRIPE_API_BASE=http://127.0.0.1:4202 and a mock Stripe API.
import assert from 'node:assert/strict';
import { createHmac } from 'node:crypto';
import { call, callErr, done, HOST, DB, newIdentity, procErr, sql, test, unwrap } from './lib.mjs';
import { startMock } from './mock-http.mjs';

const PORT = Number(process.env.STRIPE_MOCK_PORT ?? 4202);
const opt = (v) => (v === undefined ? { none: [] } : { some: v });
const unopt = (o) => (o[0] === 0 ? o[1] : undefined);
const STATUS = ['Received', 'Processed', 'Ignored', 'Failed'];
const SECRET = 'whsec_smoke_test_secret';
const CREATED = 1735000000;

// --- the mock Stripe API ----------------------------------------------------------------
let n = 0;
const subs = new Map();
const form = (body) => Object.fromEntries(new URLSearchParams(body));
const mock = await startMock(PORT, (r) => {
  const p = r.path;
  if (r.method === 'POST' && p === '/v1/customers') return { status: 200, json: { id: `cus_mock_${++n}`, object: 'customer' } };
  if (r.method === 'POST' && p === '/v1/checkout/sessions') {
    if (form(r.body)['line_items[0][price]'] === 'price_fail') {
      return { status: 402, json: { error: { type: 'card_error', code: 'card_declined', message: 'Your card\n  was declined.', request_log_url: 'https://dashboard.stripe.test/logs/1' } } };
    }
    if (form(r.body)['line_items[0][price]'] === 'price_bad_response') return { status: 200, json: { url: 'x' } };
    const id = `cs_mock_${++n}`;
    return { status: 200, json: { id, url: `https://checkout.stripe.test/${id}` } };
  }
  if (r.method === 'POST' && p === '/v1/billing_portal/sessions') return { status: 200, json: { url: 'https://billing.stripe.test/p_1' } };
  if (r.method === 'GET' && p === '/v1/prices/price_ok') return { status: 200, json: { id: 'price_ok', active: true, currency: 'usd', unit_amount: 1999, livemode: false, type: 'one_time' } };
  if (r.method === 'GET' && p.startsWith('/v1/prices/')) return { status: 404, json: { error: { type: 'invalid_request_error', code: 'resource_missing', message: 'No such price' } } };
  if (r.method === 'GET' && p === '/v1/checkout/sessions/cs_remote') {
    return { status: 200, json: { id: 'cs_remote', payment_status: 'paid', status: 'complete', mode: 'payment', amount_total: 1999, currency: 'usd', customer: { id: 'cus_remote' }, payment_intent: 'pi_remote' } };
  }
  if (r.method === 'GET' && p === '/v1/checkout/sessions/cs_gone') return { status: 404, body: 'gone' };
  const sub = p.match(/^\/v1\/subscriptions\/([^/]+)$/);
  if (sub) {
    const id = sub[1];
    if (id === 'sub_missing') return { status: 404, json: { error: { type: 'invalid_request_error', message: 'No such subscription' } } };
    const base = subs.get(id) ?? { id, object: 'subscription', customer: { id: 'cus_api' }, status: 'active', cancel_at_period_end: false, items: { data: [{ id: 'si_1', current_period_end: CREATED + 86400, quantity: 1, price: { id: 'price_api' } }] }, metadata: { userId: 'u_api', orgId: 'o_api' } };
    if (r.method === 'POST') base.cancel_at_period_end = form(r.body).cancel_at_period_end === 'true';
    if (r.method === 'DELETE') base.status = 'canceled';
    subs.set(id, base);
    return { status: 200, json: base };
  }
  if (r.method === 'POST' && p === '/v1/subscription_items/si_1') return { status: 200, json: { id: 'si_1', quantity: Number(form(r.body).quantity) } };
  return { status: 404, json: { error: { type: 'invalid_request_error', message: `no route ${r.method} ${p}` } } };
});

// --- helpers -----------------------------------------------------------------------------
const sign = (body, ts = Math.floor(Date.now() / 1000)) => `t=${ts},v1=${createHmac('sha256', SECRET).update(`${ts}.${body}`).digest('hex')}`;
const event = (id, type, object, created = CREATED) => ({ id, type, created, data: { object } });
async function ingest(payload, who, { livemode = false } = {}) {
  const body = JSON.stringify(payload);
  return call('ingest_stripe_webhook', [payload.id, payload.type, livemode, body, opt(sign(body))], who);
}
async function post(payload, { sig, body } = {}) {
  const raw = body ?? JSON.stringify(payload);
  const r = await fetch(`${HOST}/v1/database/${DB}/route/stripe/webhook`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'stripe-signature': sig ?? sign(raw) },
    body: raw,
  });
  return { status: r.status, json: await r.json().catch(() => null) };
}
const row = async (table, key, id) => (await sql(`SELECT * FROM ${table} WHERE ${key} = '${id}'`))[0];
const eventStatus = async (id) => STATUS[(await row('stripe_webhook_event', 'event_id', id))?.status[0]];
let u = 0;
const uniq = (p) => `${p}_${Date.now()}_${u++}`;

// --- before config -------------------------------------------------------------------------
await test('before config: config_not_set, relay secret_not_configured, HTTP 503', async () => {
  assert.equal(await procErr('stripe_get_or_create_customer', ['u_no_config', opt(), opt()]), 'stripe.config_not_set');
  assert.equal(await procErr('set_stripe_webhook_signing_secret', ['whsec_x']), 'stripe.config_not_set');
  const p = event('evt_pre', 'customer.created', { id: 'cus_pre' });
  assert.match(await callErr('ingest_stripe_webhook', ['evt_pre', 'customer.created', false, JSON.stringify(p), opt('t=1,v1=00')]), /stripe\.webhook_secret_not_configured/);
  const r = await post(p);
  assert.deepEqual([r.status, r.json], [503, { error: 'webhook signing secret not configured' }]);
});

const stranger = await newIdentity();
await test('admin gate: anonymous callers can not read, mutate or configure', async () => {
  for (const [name, args] of [
    ['stripe_get_customer', ['cus_1']], ['stripe_get_or_create_customer', ['u_anon', opt(), opt()]],
    ['set_stripe_config', ['sk', opt(), opt()]], ['get_stripe_config_status', []], ['stripe_get_webhook_event_count', []],
    ['validate_stripe_price', ['price_ok']], ['stripe_list_invoices', ['cus_1']],
  ]) {
    assert.equal(await procErr(name, args, stranger), 'stripe.not_authorized', name);
  }
  assert.match(await callErr('stripe_replay_webhook_event', ['evt_x'], stranger), /stripe\.not_authorized/);
  assert.match(await callErr('demo_prune_webhook_events_at', [0], stranger), /stripe\.not_authorized/);
});

await test('config: set, status, rotate the webhook secret (empty rejected), keep on None', async () => {
  unwrap(await call('set_stripe_config', ['sk_test_smoke_placeholder', opt('2025-01-01'), opt('whsec_old')]));
  assert.deepEqual(unwrap(await call('get_stripe_config_status', [])), [true, true, [0, '2025-01-01'], 25]);
  assert.equal(await procErr('set_stripe_webhook_signing_secret', ['   ']), 'stripe.invalid_webhook_signing_secret');
  unwrap(await call('set_stripe_webhook_signing_secret', [`  ${SECRET} `]));
  unwrap(await call('set_stripe_config', ['sk_test_smoke_placeholder', opt(), opt()]));
  const [cfg] = await sql('SELECT * FROM stripe_config');
  assert.equal(unopt(cfg.webhook_signing_secret), SECRET);
  assert.equal(unopt(cfg.stripe_version), '2025-01-01');
});

await test('admins: add, remove, never the last', async () => {
  const other = await newIdentity();
  unwrap(await call('stripe_add_admin_identity', [{ __identity__: '0x' + other.identity }]));
  unwrap(await call('get_stripe_config_status', [], other));
  unwrap(await call('stripe_remove_admin_identity', [{ __identity__: '0x' + other.identity }], other));
  assert.equal(await procErr('get_stripe_config_status', [], other), 'stripe.not_authorized');
  const [me] = await sql('SELECT * FROM stripe_admin_identity');
  assert.equal(await procErr('stripe_remove_admin_identity', [me.identity]), 'stripe.cannot_remove_last_admin');
});

await test('request validation (upstream unit cases) through the stripe_request helper', async () => {
  const req = (m, p, b, k) => call('demo_stripe_request', [m, p, opt(b), opt(k)]);
  assert.equal(await procErr('demo_stripe_request', ['GET', 'https://attacker.example/collect', opt(), opt()]), 'stripe.request_path_invalid');
  assert.equal(await procErr('demo_stripe_request', ['GET', '/v2/customers', opt(), opt()]), 'stripe.request_path_invalid');
  assert.equal(await procErr('demo_stripe_request', ['GET', '/v1/customers\u007fblocked', opt(), opt()]), 'stripe.request_path_invalid');
  assert.equal(await procErr('demo_stripe_request', ['GET', '/v1/x#y', opt(), opt()]), 'stripe.request_path_invalid');
  assert.equal(await procErr('demo_stripe_request', ['TRACE', '/v1/customers', opt(), opt()]), 'stripe.request_method_invalid');
  assert.equal(await procErr('demo_stripe_request', ['PATCH', '/v1/customers', opt(), opt()]), 'stripe.request_method_invalid');
  assert.equal(await procErr('demo_stripe_request', ['POST', '/v1/customers', opt('x'.repeat(64 * 1024 + 1)), opt()]), 'stripe.request_body_too_large');
  assert.equal(await procErr('demo_stripe_request', ['POST', '/v1/customers', opt(), opt('k'.repeat(256))]), 'stripe.idempotency_key_too_long');
  mock.clear();
  const res = unwrap(await req(' post ', ' /v1/customers ', 'email=user%40example.com', 'customer-user-1'));
  assert.equal(res[0], 200);
  const r = mock.last();
  assert.equal(r.method, 'POST');
  assert.equal(r.path, '/v1/customers');
  assert.equal(r.headers.authorization, 'Bearer sk_test_smoke_placeholder');
  assert.equal(r.headers['stripe-version'], '2025-01-01');
  assert.equal(r.headers['idempotency-key'], 'customer-user-1');
  assert.equal(r.headers['content-type'], 'application/x-www-form-urlencoded');
  assert.equal(r.body, 'email=user%40example.com');
});

// --- webhooks: HTTP route ------------------------------------------------------------------
await test('HTTP: failed payloads stay retryable; valid and ignored are acknowledged twice', async () => {
  for (const [id, type, object, expected] of [
    ['evt_http_invalid', 'customer.created', {}, 400],
    ['evt_http_valid', 'customer.created', { id: 'cus_http_valid' }, 200],
    ['evt_http_ignored', 'unhandled.event', {}, 200],
  ]) {
    for (let attempt = 0; attempt < 2; attempt++) {
      const r = await post(event(id, type, object));
      assert.equal(r.status, expected, `${id} attempt ${attempt}`);
      assert.equal(r.json.eventId, id);
      assert.equal(r.json.kind, attempt === 1 && expected === 200 ? 'duplicate' : 'applied');
    }
  }
  assert.equal(await eventStatus('evt_http_invalid'), 'Failed');
  assert.equal(await eventStatus('evt_http_ignored'), 'Ignored');
  const paid = event('evt_http_paid', 'invoice.paid', { id: 'in_http_retry', customer: null });
  assert.equal((await post(paid)).status, 400);
  await ingest(event('evt_http_invoice', 'invoice.created', { id: 'in_http_retry', customer: 'cus_http_valid', status: 'open' }));
  const ok = await post(paid);
  assert.equal(ok.status, 200);
  assert.deepEqual(ok.json, { ok: true, eventId: 'evt_http_paid', kind: 'applied', status: { tag: 'Processed' } });
  const invoices = unwrap(await call('stripe_list_invoices', ['cus_http_valid']));
  assert.equal(invoices[0][3], 'paid');
});

await test('HTTP: signature, size and metadata rejections', async () => {
  const p = event(uniq('evt'), 'customer.created', { id: 'cus_sig' });
  const body = JSON.stringify(p);
  assert.deepEqual((await post(p, { sig: `t=${Math.floor(Date.now() / 1000)},v1=${'0'.repeat(64)}` })).json, { error: 'crypto.signature_mismatch' });
  assert.equal((await post(p, { sig: sign(body, Math.floor(Date.now() / 1000) - 3600) })).json.error, 'crypto.timestamp_outside_tolerance');
  assert.equal((await post(p, { sig: 'garbage' })).status, 401);
  assert.equal((await post(p, { sig: 'x'.repeat(8193) })).status, 431);
  assert.deepEqual(await post(p, { body: '' }), { status: 400, json: { error: 'empty body' } });
  const noId = JSON.stringify({ type: 'customer.created', data: { object: {} } });
  assert.deepEqual(await post(null, { body: noId }), { status: 400, json: { error: 'missing or invalid event metadata' } });
  const emptyId = JSON.stringify({ id: '', type: 'customer.created' });
  assert.deepEqual(await post(null, { body: emptyId }), { status: 400, json: { error: 'invalid event metadata' } });
  assert.equal(await eventStatus(p.id), undefined);
});

// --- webhooks: relay reducer ----------------------------------------------------------------
await test('relay keeps a Failed row for an invalid signed payload; replay keeps it Failed', async () => {
  const p = event('evt_relay_invalid', 'customer.created', {});
  for (let attempt = 0; attempt < 2; attempt++) {
    await ingest(p);
    assert.equal(await eventStatus('evt_relay_invalid'), 'Failed');
  }
  await call('stripe_replay_webhook_event', ['evt_relay_invalid']);
  assert.equal(await eventStatus('evt_relay_invalid'), 'Failed');
  assert.match(unopt((await row('stripe_webhook_event', 'event_id', 'evt_relay_invalid')).error_message), /^payload validation failed: /);
});

await test('relay: signed metadata must match; size and signature limits; bad signature writes nothing', async () => {
  const body = JSON.stringify(event('evt_smoke_signed_id', 'customer.created', { id: 'cus_should_not_exist' }));
  assert.match(await callErr('ingest_stripe_webhook', ['evt_smoke_forged_id', 'customer.deleted', false, body, opt(sign(body))]), /stripe\.webhook_metadata_mismatch/);
  assert.match(await callErr('ingest_stripe_webhook', ['evt_smoke_signed_id', 'customer.created', true, body, opt(sign(body))]), /stripe\.webhook_metadata_mismatch/);
  assert.match(await callErr('ingest_stripe_webhook', ['', 'customer.created', false, body, opt(sign(body))]), /stripe\.webhook_metadata_invalid/);
  assert.match(await callErr('ingest_stripe_webhook', ['evt_x', 'customer.created', false, body, opt('x'.repeat(8193))]), /stripe\.webhook_signature_too_large/);
  assert.match(await callErr('ingest_stripe_webhook', ['evt_smoke_signed_id', 'customer.created', false, body, opt(`t=${Math.floor(Date.now() / 1000)},v1=${'0'.repeat(64)}`)]), /stripe\.webhook_signature_mismatch:crypto\.signature_mismatch/);
  const noMeta = JSON.stringify({ type: 'customer.created' });
  assert.match(await callErr('ingest_stripe_webhook', ['evt_y', 'customer.created', false, noMeta, opt(sign(noMeta))]), /stripe\.webhook_payload_missing_metadata/);
  assert.equal(await row('stripe_customer', 'stripe_customer_id', 'cus_should_not_exist'), undefined);
  assert.equal(await eventStatus('evt_smoke_signed_id'), undefined);
});

await test('a non-integer timestamp aborts the whole ingest, as the TS BigInt() RangeError does', async () => {
  const p = event('evt_float', 'customer.created', { id: 'cus_float' }, 1.5);
  assert.ok(await callErr('ingest_stripe_webhook', [p.id, p.type, false, JSON.stringify(p), opt(sign(JSON.stringify(p)))]));
  assert.equal(await eventStatus('evt_float'), undefined);
  const r = await post(p);
  assert.deepEqual([r.status, r.json], [500, { error: 'webhook processing failed' }]);
  assert.equal(await eventStatus('evt_float'), undefined);
});

// --- the upstream smoke test, event by event --------------------------------------------------
await test('customer.created → customer row with metadata userId', async () => {
  await ingest(event('evt_smoke_cust_1', 'customer.created', { id: 'cus_smoke_1', email: 'smoke@example.com', name: 'Smoke Test', metadata: { userId: 'u_smoke_1' } }));
  const c = unwrap(await call('stripe_get_customer', ['cus_smoke_1']));
  assert.equal(c[0], 0);
  const r = await row('stripe_customer', 'stripe_customer_id', 'cus_smoke_1');
  assert.equal(unopt(r.email), 'smoke@example.com');
  assert.equal(unopt(r.user_id), 'u_smoke_1');
  assert.equal(unopt(r.metadata_json), '{"userId":"u_smoke_1"}');
  assert.equal(r.event_created_unix, CREATED);
  const byUser = unwrap(await call('stripe_get_customer_by_user_id', ['u_smoke_1']));
  assert.equal(byUser[1][0], 'cus_smoke_1');
});

await test('customer.subscription.created → subscription row with item fields and metadata', async () => {
  await ingest(event('evt_smoke_sub_1', 'customer.subscription.created', {
    id: 'sub_smoke_1', customer: 'cus_smoke_1', status: 'active', current_period_end: 1735000000, cancel_at_period_end: false,
    items: { data: [{ current_period_end: 1735003600, quantity: 1, price: { id: 'price_smoke_1' } }] },
    metadata: { userId: 'u_smoke_1', orgId: 'o_smoke_1' },
  }));
  const r = await row('stripe_subscription', 'stripe_subscription_id', 'sub_smoke_1');
  assert.equal(r.status, 'active');
  assert.equal(r.current_period_end_unix, 1735003600, 'the first item wins over the top-level period end');
  assert.equal(unopt(r.quantity), 1);
  assert.equal(unopt(r.price_id), 'price_smoke_1');
  assert.equal(unopt(r.org_id), 'o_smoke_1');
  assert.equal(unwrap(await call('stripe_get_subscription_by_org_id', ['o_smoke_1']))[1][0], 'sub_smoke_1');
  assert.equal(unwrap(await call('stripe_list_subscriptions', ['cus_smoke_1'])).length, 1);
  assert.equal(unwrap(await call('stripe_list_subscriptions_by_user_id', ['u_smoke_1'])).length, 1);
  assert.equal(unwrap(await call('stripe_list_subscriptions_by_org_id', ['o_smoke_1'])).length, 1);
});

await test('cancel_at near the period end derives cancel_at_period_end', async () => {
  await ingest(event('evt_sub_derive', 'customer.subscription.created', { id: 'sub_derive', customer: { id: 'cus_d' }, status: 'active', current_period_end: 2000, cancel_at: 2100 }));
  const r = await row('stripe_subscription', 'stripe_subscription_id', 'sub_derive');
  assert.equal(r.cancel_at_period_end, true);
  assert.equal(r.stripe_customer_id, 'cus_d', 'expandable customer object');
});

await test('checkout.session.completed → session row', async () => {
  await ingest(event('evt_smoke_chk_1', 'checkout.session.completed', { id: 'cs_smoke_1', status: 'complete', payment_status: 'paid', mode: 'subscription', customer: 'cus_smoke_1', metadata: { userId: 'u_smoke_1' } }));
  const s = await row('stripe_checkout_session', 'stripe_checkout_session_id', 'cs_smoke_1');
  assert.equal(s.status, 'complete');
  assert.equal(s.mode, 'subscription');
  assert.equal(unwrap(await call('stripe_get_checkout_session', ['cs_smoke_1']))[1][3], 'paid');
  assert.equal(unwrap(await call('stripe_list_checkout_sessions', ['cus_smoke_1'])).length, 1);
});

await test('invoice.created then invoice.paid (carry-over); late invoice.finalized can not reopen it', async () => {
  await ingest(event('evt_smoke_inv_1', 'invoice.created', { id: 'in_smoke_1', customer: 'cus_smoke_1', subscription: 'sub_smoke_1', status: 'open', amount_due: 999, amount_paid: 0, created: 1735000000 }));
  await ingest(event('evt_smoke_inv_2', 'invoice.paid', { id: 'in_smoke_1', customer: 'cus_smoke_1', status: 'paid', amount_paid: 999 }));
  let inv = await row('stripe_invoice', 'stripe_invoice_id', 'in_smoke_1');
  assert.equal(inv.status, 'paid');
  assert.equal(inv.amount_due, 999, 'carried over');
  assert.equal(inv.amount_paid, 999);
  assert.equal(unopt(inv.stripe_subscription_id), 'sub_smoke_1');
  assert.equal(unopt(inv.org_id), 'o_smoke_1', 'org from the subscription row');
  await ingest(event('evt_smoke_inv_late', 'invoice.finalized', { id: 'in_smoke_1', customer: 'cus_smoke_1', status: 'open' }));
  inv = await row('stripe_invoice', 'stripe_invoice_id', 'in_smoke_1');
  assert.equal(inv.status, 'paid');
  assert.equal(await eventStatus('evt_smoke_inv_late'), 'Ignored');
  assert.equal(unwrap(await call('stripe_list_invoices_by_org_id', ['o_smoke_1'])).length, 1);
  assert.equal(unwrap(await call('stripe_list_invoices_by_user_id', ['u_smoke_1'])).length, 1);
});

await test('late customer.subscription.updated can not revive a canceled subscription', async () => {
  for (const [id, type, created, status] of [
    ['evt_smoke_sub_del', 'customer.subscription.deleted', 20, 'canceled'],
    ['evt_smoke_sub_late', 'customer.subscription.updated', 10, 'active'],
  ]) {
    await ingest(event(id, type, { id: 'sub_smoke_1', customer: 'cus_smoke_1', status }, CREATED + created));
  }
  assert.equal((await row('stripe_subscription', 'stripe_subscription_id', 'sub_smoke_1')).status, 'canceled');
  // a newer "active" can't move it backward either
  await ingest(event('evt_sub_newer', 'customer.subscription.updated', { id: 'sub_smoke_1', customer: 'cus_smoke_1', status: 'active' }, CREATED + 99));
  assert.equal((await row('stripe_subscription', 'stripe_subscription_id', 'sub_smoke_1')).status, 'canceled');
  assert.equal(unopt((await row('stripe_subscription', 'stripe_subscription_id', 'sub_smoke_1')).org_id), 'o_smoke_1', 'metadata kept');
});

await test('payment_intent.succeeded standalone → payment row', async () => {
  await ingest(event('evt_smoke_pay_1', 'payment_intent.succeeded', { id: 'pi_smoke_1', customer: 'cus_smoke_2', amount: 1999, currency: 'usd', status: 'succeeded', created: 1735000100, metadata: { userId: 'u_smoke_2', orgId: 'o_smoke_2' } }));
  const p = await row('stripe_payment', 'stripe_payment_intent_id', 'pi_smoke_1');
  assert.equal(p.amount, 1999);
  assert.equal(p.currency, 'usd');
  assert.equal(p.created_unix, 1735000100);
  assert.equal(unwrap(await call('stripe_list_payments', ['cus_smoke_2'])).length, 1);
  assert.equal(unwrap(await call('stripe_list_payments_by_user_id', ['u_smoke_2'])).length, 1);
  assert.equal(unwrap(await call('stripe_list_payments_by_org_id', ['o_smoke_2'])).length, 1);
  assert.equal(unwrap(await call('stripe_get_payment', ['pi_smoke_1']))[1][4], 'usd');
});

await test('invoice_payment.paid before its invoice: the invoice fills the payment row in', async () => {
  await ingest(event('evt_smoke_inpay_early', 'invoice_payment.paid', { invoice: 'in_smoke_early', amount_paid: 999, currency: 'usd', created: CREATED, payment: { payment_intent: 'pi_smoke_early' } }));
  let p = await row('stripe_payment', 'stripe_payment_intent_id', 'pi_smoke_early');
  assert.equal(unopt(p.stripe_customer_id), undefined);
  assert.equal(unopt(p.stripe_invoice_id), 'in_smoke_early');
  await ingest(event('evt_smoke_inv_early', 'invoice.paid', { id: 'in_smoke_early', customer: 'cus_smoke_1', parent: { subscription_details: { subscription: 'sub_smoke_1' } }, status: 'paid', amount_paid: 999 }));
  p = await row('stripe_payment', 'stripe_payment_intent_id', 'pi_smoke_early');
  assert.equal(unopt(p.stripe_customer_id), 'cus_smoke_1');
  assert.equal(unopt(p.user_id), 'u_smoke_1');
  // without a payment_intent the event is acknowledged and ignored
  await ingest(event('evt_inpay_nopi', 'invoice_payment.paid', { invoice: 'in_x', amount_paid: null, currency: 'usd', created: CREATED, payment: {} }));
  assert.equal(await eventStatus('evt_inpay_nopi'), 'Ignored');
});

await test('invoice before its subscription: user from the invoice parent metadata', async () => {
  await ingest(event('evt_smoke_inv_nosub', 'invoice.paid', { id: 'in_smoke_nosub', customer: 'cus_smoke_3', parent: { subscription_details: { subscription: 'sub_smoke_later', metadata: { userId: 'u_smoke_3' } } }, status: 'paid' }));
  const inv = await row('stripe_invoice', 'stripe_invoice_id', 'in_smoke_nosub');
  assert.equal(unopt(inv.user_id), 'u_smoke_3');
  assert.equal(unopt(inv.stripe_subscription_id), 'sub_smoke_later');
});

await test('checkout.session.async_payment_failed records paymentStatus failed; a paid session never fails', async () => {
  for (const [id, type] of [['evt_smoke_chk_async', 'checkout.session.completed'], ['evt_smoke_chk_failed', 'checkout.session.async_payment_failed']]) {
    await ingest(event(id, type, { id: 'cs_smoke_async', status: 'complete', payment_status: 'unpaid', mode: 'payment' }));
  }
  assert.equal((await row('stripe_checkout_session', 'stripe_checkout_session_id', 'cs_smoke_async')).payment_status, 'failed');
  await ingest(event('evt_chk_fail_paid', 'checkout.session.async_payment_failed', { id: 'cs_smoke_1', payment_status: 'paid' }, CREATED + 50));
  assert.equal((await row('stripe_checkout_session', 'stripe_checkout_session_id', 'cs_smoke_1')).payment_status, 'paid');
});

await test('replay of an unknown event fails; re-ingesting a processed event changes nothing', async () => {
  assert.match(await callErr('stripe_replay_webhook_event', ['evt_does_not_exist_xyz']), /stripe\.webhook_event_not_found/);
  await ingest(event('evt_smoke_cust_1', 'customer.created', { id: 'cus_smoke_1', email: 'CHANGED@example.com' }));
  assert.equal(unopt((await row('stripe_customer', 'stripe_customer_id', 'cus_smoke_1')).email), 'smoke@example.com');
  await call('stripe_replay_webhook_event', ['evt_smoke_inv_2']);
  assert.equal((await row('stripe_invoice', 'stripe_invoice_id', 'in_smoke_1')).status, 'paid');
});

await test('older customer events are ignored; metadata keys are written in JS order', async () => {
  await ingest(event('evt_cust_old', 'customer.updated', { id: 'cus_smoke_1', email: 'old@example.com' }, CREATED - 10));
  assert.equal(await eventStatus('evt_cust_old'), 'Ignored');
  await ingest(event('evt_cust_keys', 'customer.created', { id: 'cus_keys', metadata: { b: '1', 10: 'ten', a: '2', 2: 'two' } }));
  assert.equal(unopt((await row('stripe_customer', 'stripe_customer_id', 'cus_keys')).metadata_json), '{"2":"two","10":"ten","b":"1","a":"2"}');
});

// --- billing --------------------------------------------------------------------------------
await test('get_or_create_customer: creates once with metadata and an idempotency key, then reuses', async () => {
  mock.clear();
  const first = unwrap(await call('stripe_get_or_create_customer', ['u_new', opt('new@example.com'), opt('New Person')]));
  assert.equal(first[1], true);
  const r = mock.last();
  assert.equal(r.body, 'email=new%40example.com&name=New%20Person&metadata%5BuserId%5D=u_new');
  assert.equal(r.headers['idempotency-key'], 'create_customer_u_new');
  const c = await row('stripe_customer', 'stripe_customer_id', first[0]);
  assert.equal(unopt(c.user_id), 'u_new');
  assert.equal(unopt(c.metadata_json), '{"userId":"u_new"}');
  const second = unwrap(await call('stripe_get_or_create_customer', ['u_new', opt(), opt()]));
  assert.deepEqual(second, [first[0], false]);
  assert.deepEqual(unwrap(await call('stripe_get_or_create_customer', ['u_smoke_1', opt(), opt()])), ['cus_smoke_1', false]);
});

await test('create_checkout_session: form body, metadata by mode, errors', async () => {
  mock.clear();
  const items = [{ price_id: 'price_a', quantity: 2 }, { price_id: 'price_b', quantity: 1 }];
  const res = unwrap(await call('stripe_create_checkout_session', [items, opt('cus_smoke_1'), 'subscription', 'https://a.test/ok?x=1&y=2', 'https://a.test/no', opt('{"plan":"pro","userId":"u1"}'), opt('{"seats":3,"flag":true,"skip":null}'), opt('{"ignored":"yes"}')]));
  assert.match(res[0], /^cs_mock_/);
  assert.equal(unopt(res[1]), `https://checkout.stripe.test/${res[0]}`);
  assert.equal(
    mock.last().body,
    'mode=subscription&success_url=https%3A%2F%2Fa.test%2Fok%3Fx%3D1%26y%3D2&cancel_url=https%3A%2F%2Fa.test%2Fno&customer=cus_smoke_1' +
      '&line_items%5B0%5D%5Bprice%5D=price_a&line_items%5B0%5D%5Bquantity%5D=2&line_items%5B1%5D%5Bprice%5D=price_b&line_items%5B1%5D%5Bquantity%5D=1' +
      '&metadata%5Bplan%5D=pro&metadata%5BuserId%5D=u1&subscription_data%5Bmetadata%5D%5Bseats%5D=3&subscription_data%5Bmetadata%5D%5Bflag%5D=true'
  );
  assert.equal(await procErr('stripe_create_checkout_session', [[], opt(), 'payment', 'a', 'b', opt(), opt(), opt()]), 'stripe.checkout_session_requires_items');
  assert.equal(
    await procErr('stripe_create_checkout_session', [[{ price_id: 'price_fail', quantity: 1 }], opt(), 'payment', 'a', 'b', opt(), opt(), opt()]),
    'stripe.checkout_session_failed:402:type=card_error|code=card_declined|msg=Your card was declined.|log=https://dashboard.stripe.test/logs/1'
  );
  assert.match(await procErr('stripe_create_checkout_session', [[{ price_id: 'price_bad_response', quantity: 1 }], opt(), 'payment', 'a', 'b', opt(), opt(), opt()]), /^stripe\.checkout_session_invalid_response:/);
});

await test('portal session, price validation, remote checkout session', async () => {
  mock.clear();
  assert.deepEqual(unwrap(await call('stripe_create_customer_portal_session', ['cus_smoke_1', 'https://a.test/account'])), ['https://billing.stripe.test/p_1']);
  assert.equal(mock.last().body, 'customer=cus_smoke_1&return_url=https%3A%2F%2Fa.test%2Faccount');
  assert.deepEqual(unwrap(await call('validate_stripe_price', ['price_ok'])), [true, 200, [0, true], [0, 'usd'], [0, 1999], [0, false], [0, 'one_time'], [1, []], [1, []], [1, []]]);
  assert.deepEqual(unwrap(await call('validate_stripe_price', ['price_missing'])), [false, 404, [1, []], [1, []], [1, []], [1, []], [1, []], [0, 'No such price'], [0, 'resource_missing'], [0, 'invalid_request_error']]);
  const remote = unwrap(await call('stripe_get_remote_checkout_session', ['cs_remote']));
  assert.deepEqual(remote.slice(0, 10), [true, 200, [0, 'cs_remote'], [0, 'paid'], [0, 'complete'], [0, 'payment'], [0, 1999], [0, 'usd'], [0, 'cus_remote'], [0, 'pi_remote']]);
  const gone = unwrap(await call('stripe_get_remote_checkout_session', ['cs_gone']));
  assert.deepEqual([gone[0], gone[1], unopt(gone[10])], [false, 404, 'Stripe returned 404.']);
});

await test('subscriptions: cancel at period end, cancel now, reactivate, quantity, metadata', async () => {
  mock.clear();
  unwrap(await call('stripe_cancel_subscription', ['sub_api', opt()]));
  assert.deepEqual([mock.last().method, mock.last().body], ['POST', 'cancel_at_period_end=true']);
  let s = await row('stripe_subscription', 'stripe_subscription_id', 'sub_api');
  assert.equal(s.cancel_at_period_end, true);
  assert.equal(s.stripe_customer_id, 'cus_api');
  assert.equal(unopt(s.user_id), 'u_api');
  unwrap(await call('stripe_reactivate_subscription', ['sub_api']));
  assert.equal((await row('stripe_subscription', 'stripe_subscription_id', 'sub_api')).cancel_at_period_end, false);
  unwrap(await call('stripe_update_subscription_quantity', ['sub_api', 5]));
  assert.deepEqual([mock.last().path, mock.last().body], ['/v1/subscription_items/si_1', 'quantity=5']);
  assert.equal(unopt((await row('stripe_subscription', 'stripe_subscription_id', 'sub_api')).quantity), 5);
  unwrap(await call('stripe_cancel_subscription', ['sub_api', opt(false)]));
  assert.equal(mock.last().method, 'DELETE');
  assert.equal((await row('stripe_subscription', 'stripe_subscription_id', 'sub_api')).status, 'canceled');
  unwrap(await call('stripe_update_subscription_metadata', ['sub_api', '{"orgId":"o_new","tier":"gold"}', opt(), opt('u_override')]));
  s = await row('stripe_subscription', 'stripe_subscription_id', 'sub_api');
  assert.deepEqual([unopt(s.metadata_json), unopt(s.org_id), unopt(s.user_id)], ['{"orgId":"o_new","tier":"gold"}', 'o_new', 'u_override']);
  assert.equal(await procErr('stripe_update_subscription_metadata', ['sub_nope', '{}', opt(), opt()]), 'stripe.subscription_not_found');
  assert.equal(await procErr('stripe_cancel_subscription', ['sub_missing', opt()]), 'stripe.subscription_update_failed:404:type=invalid_request_error|msg=No such subscription');
  assert.equal(await procErr('stripe_update_subscription_quantity', ['sub_missing', 2]), 'stripe.subscription_lookup_failed:404:type=invalid_request_error|msg=No such subscription');
});

await test('host checkout procedure: any user, server-owned prices, userId in metadata', async () => {
  const shopper = await newIdentity();
  mock.clear();
  const res = unwrap(await call('create_store_checkout_session', ['premium'], shopper));
  assert.match(unopt(res[0]), /^https:\/\/checkout\.stripe\.test\/cs_mock_/);
  const [customerCall, sessionCall] = mock.requests;
  assert.equal(form(customerCall.body)['metadata[userId]'], shopper.identity);
  const f = form(sessionCall.body);
  assert.equal(f['line_items[0][price]'], 'price_premium');
  assert.equal(f['metadata[userId]'], shopper.identity);
  assert.equal(f['payment_intent_data[metadata][userId]'], shopper.identity);
  assert.equal(f['subscription_data[metadata][userId]'], undefined);
  assert.equal(f.success_url, 'https://store.example.com/?checkout=success');
  assert.equal(await procErr('create_store_checkout_session', ['retired'], shopper), 'store.product_unavailable');
  // fulfilment comes from webhook state
  const sessionId = unopt(res[0]).split('/').pop();
  await ingest(event(uniq('evt_paid'), 'checkout.session.completed', { id: sessionId, payment_status: 'paid', mode: 'payment', metadata: { userId: shopper.identity } }));
  assert.deepEqual(await call('my_paid_sessions', [], shopper), [sessionId]);
});

await test('retention: events older than 30 days are pruned, 500 per run; count procedure', async () => {
  const before = unwrap(await call('stripe_get_webhook_event_count', []));
  assert.ok(before > 10);
  await call('demo_prune_webhook_events_at', [(Date.now() + 29 * 86400_000) * 1000]);
  assert.equal(unwrap(await call('stripe_get_webhook_event_count', [])), before);
  await call('demo_prune_webhook_events_at', [(Date.now() + 31 * 86400_000) * 1000]);
  assert.equal(unwrap(await call('stripe_get_webhook_event_count', [])), 0);
  const r = await fetch(`${HOST}/v1/database/${DB}/call/stripe_prune_webhook_events`, { method: 'POST', body: '[{"scheduled_id":1,"scheduled_at":{"Interval":{"__time_duration_micros__":1}}}]' });
  assert.ok(!r.ok, 'the scheduled reducer is not callable by clients');
  const [tick] = await sql('SELECT * FROM stripe_webhook_prune_tick');
  assert.ok(tick, 'install scheduled the sweep');
});

await mock.close();
done();
