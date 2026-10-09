# stripe: Checkout, subscriptions, invoices and payments, mirrored from Stripe webhooks

Port of [`@spacetimedb/stripe`](https://github.com/clockworklabs/SpacetimeDB/pull/5906) at
`16396207cea1a07f4ac476864a05ffcfd19bfc25`. Needs [`crypto`](../crypto/) for the `Stripe-Signature` check.

**Schema (same as TS):** private tables `stripe_customer`, `stripe_subscription`, `stripe_checkout_session`,
`stripe_payment`, `stripe_invoice`, `stripe_webhook_event`, `stripe_webhook_prune_tick` (scheduled),
`stripe_config` and `stripe_admin_identity`.
Behaviour: same form encoding and `Stripe-Version` header, idempotency keys, error codes, webhook limits (413/431),
5-minute signature tolerance, event dedupe and replay, and stale-event rejection. Status changes only move
forward (subscription, invoice and checkout ranks), and webhook events are pruned on the schedule.

## Rust

Copy [`rust/stripe.rs`](rust/stripe.rs) and [`../crypto/rust/crypto.rs`](../crypto/rust/crypto.rs) (as `crate::crypto`;
dependencies `sha2`, `hmac`, `log`, `serde_json` with `preserve_order`), then:

```rust
#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) { stripe::install(ctx); }

#[router]
fn routes() -> Router { Router::new().post("/stripe/webhook", stripe::stripe_webhook) }

// A host procedure: resolve the user from ctx.sender(), take prices from your own catalog.
let session = stripe::create_user_checkout_session(ctx, stripe::UserCheckoutArgs {
    user_id, items: vec![StripeCheckoutLineItem { price_id, quantity: 1 }], mode: "payment".into(),
    success_url, cancel_url, ..Default::default()
})?;
```

## C#

Add [`csharp/Stripe.cs`](csharp/Stripe.cs) and [`../crypto/csharp/Crypto.cs`](../crypto/csharp/Crypto.cs), then:

```csharp
[Reducer(ReducerKind.Init)]
public static void Init(ReducerContext ctx) => StripeInstall(ctx);

[SpacetimeDB.HttpRouter]
public static Router Routes() => Router.New().Post("/stripe/webhook", Handlers.StripeWebhook);

// In a host procedure (throws on failure):
var session = StripeCreateUserCheckoutSession(ctx, new StripeUserCheckoutArgs
{
    UserId = userId, Items = [new StripeCheckoutLineItem { PriceId = priceId, Quantity = 1 }],
    Mode = "payment", SuccessUrl = successUrl, CancelUrl = cancelUrl,
});
```

| TypeScript | Rust | C# |
|---|---|---|
| `install(ctx)` | `stripe::install(ctx)` | `StripeInstall(ctx)` |
| `stripeWebhookHandler` / `stripeWebhookRouter` | `stripe::stripe_webhook` | `StripeWebhook` |
| `getOrCreateUserCustomer`, `createUserCheckoutSession` | `get_or_create_user_customer`, `create_user_checkout_session` | `StripeGetOrCreateUserCustomer`, `StripeCreateUserCheckoutSession` |
| `stripeRequest` (and the internal `createCheckoutSessionInStripe`, public here) | `create_checkout_session_in_stripe`, `stripe_request` | `StripeCreateCheckoutSessionInStripe`, `StripeRequest` |
| `setStripeConfig`, `setStripeWebhookSigningSecret`, `getStripeConfigStatus` | same, snake_case | same, PascalCase |
| `validateStripePrice`, `ingestStripeWebhook` | `validate_stripe_price`, `ingest_stripe_webhook` | `ValidateStripePrice`, `IngestStripeWebhook` |
| `addAdminIdentity`, `removeAdminIdentity` | `stripe_add_admin_identity`, … | `StripeAddAdminIdentity`, … |
| billing (`getOrCreateCustomer`, `createCheckoutSession`, `cancelSubscription`, …) | `stripe_get_or_create_customer`, `stripe_create_checkout_session`, … | `StripeGetOrCreateCustomer`, … |
| queries (`getCustomer`, `listSubscriptionsByOrgId`, `listInvoices`, …) | `stripe_get_customer`, … | `StripeGetCustomer`, … |
| `replayWebhookEvent`, `pruneWebhookEvents` | `stripe_replay_webhook_event`, `stripe_prune_webhook_events` | `StripeReplayWebhookEvent`, `StripePruneWebhookEvents` |

**Differences:**
- **Names:** generic procedures and reducers get a `stripe_` prefix, and generic types a `Stripe` prefix
  (`CheckoutLineItem` → `StripeCheckoutLineItem`, …). Names already containing "stripe" are unchanged.
- **Results:** procedures return `Result<T, String>` instead of throwing.
- **`ValidateStripePriceResult.price_type`:** the TS field is `type`. Rust's `SpacetimeType` derive keeps the
  `r#` of a raw identifier, so the field can't be called `type` (see [FINDINGS](../docs/FINDINGS.md)). Both ports
  use `price_type` so their schemas match.
- **Scans:** lookups by user, by org, or by an optional customer/invoice id scan the table, because Rust can't
  filter an `Option` index. The indexes still exist, for parity. Lists sort by `inserted_at` as the TS indexes
  do; ties come back in no fixed order.
- **Validation messages:** payload validation is hand-written (no valibot), so `Failed` rows carry differently
  worded messages. A non-integer number where TS calls `BigInt()` aborts the transaction, as the TS
  `RangeError` does.
- **Scheduler-only:** `stripe_prune_webhook_events` rejects callers other than the scheduler.
- **Test base URL:** build with `STRIPE_API_BASE=http://127.0.0.1:4202` to target a mock. Rust uses
  `option_env!`; C# uses the demo csproj's generated `StripeOverrideApiBase` partial. Production builds always
  use `https://api.stripe.com`.

**Tests:** [`tests/stripe.test.mjs`](../tests/stripe.test.mjs) has 28 cases ported from the upstream suite: config and
admins, form encoding and headers, customers and Checkout (the demo's catalog-backed host procedure), price
validation, the portal, subscription updates, signed webhooks (limits, tolerance, bad signatures), every handled
event type, out-of-order and stale events, dedupe, replay and pruning. All run against a local mock Stripe; nothing
calls Stripe.
