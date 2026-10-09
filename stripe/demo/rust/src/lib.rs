//! Demo for stripe.rs: the standalone package's procedures and reducers, the webhook route at
//! `/stripe/webhook`, and the upstream "Premium Store" pattern: a server-owned product catalog
//! and a host checkout procedure that any signed-in user may call.
//!
//! Build with `STRIPE_API_BASE=http://127.0.0.1:4202` to talk to the test suite's mock API.

#[path = "../../../../crypto/rust/crypto.rs"]
pub mod crypto;
#[path = "../../../rust/stripe.rs"]
pub mod stripe;

use spacetimedb::http::{Router, router};
use spacetimedb::{ProcedureContext, ReducerContext, SpacetimeType, Table, Timestamp};
use stripe::{StripeCheckoutLineItem, stripe_checkout_session as _};

/// The server-owned catalog: clients pick a product, never a price.
#[spacetimedb::table(accessor = store_product)]
pub struct StoreProduct {
    #[primary_key]
    pub product_id: String,
    pub stripe_price_id: Option<String>,
    pub active: bool,
}

#[derive(SpacetimeType)]
pub struct StoreCheckoutSession {
    pub url: Option<String>,
}

const APP_ORIGIN: &str = "https://store.example.com";

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    stripe::install(ctx);
    for (id, price, active) in [
        ("premium", Some("price_premium"), true),
        ("retired", Some("price_old"), false),
    ] {
        ctx.db.store_product().insert(StoreProduct {
            product_id: id.into(),
            stripe_price_id: price.map(str::to_string),
            active,
        });
    }
}

#[router]
fn routes() -> Router {
    Router::new().post("/stripe/webhook", stripe::stripe_webhook)
}

/// Anyone may start Checkout for an active product, as themselves.
#[spacetimedb::procedure]
pub fn create_store_checkout_session(
    ctx: &mut ProcedureContext,
    product_id: String,
) -> Result<StoreCheckoutSession, String> {
    let price = ctx.with_tx(|tx| {
        tx.db
            .store_product()
            .product_id()
            .find(&product_id)
            .filter(|p| p.active)
            .and_then(|p| p.stripe_price_id)
    });
    let Some(price_id) = price else {
        return Err("store.product_unavailable".into());
    };
    let session = stripe::create_user_checkout_session(
        ctx,
        stripe::UserCheckoutArgs {
            user_id: ctx.sender().to_hex().to_string(),
            items: vec![StripeCheckoutLineItem {
                price_id,
                quantity: 1,
            }],
            mode: "payment".into(),
            success_url: format!("{APP_ORIGIN}/?checkout=success"),
            cancel_url: format!("{APP_ORIGIN}/?checkout=cancelled"),
            ..Default::default()
        },
    )?;
    Ok(StoreCheckoutSession { url: session.url })
}

/// The caller's own paid Checkout sessions (fulfil from webhook state, not the redirect).
#[spacetimedb::procedure]
pub fn my_paid_sessions(ctx: &mut ProcedureContext) -> Vec<String> {
    let me = ctx.sender().to_hex().to_string();
    let needle = format!("\"userId\":\"{me}\"");
    ctx.with_tx(|tx| {
        tx.db
            .stripe_checkout_session()
            .iter()
            .filter(|s| matches!(s.payment_status.as_str(), "paid" | "no_payment_required"))
            .filter(|s| {
                s.metadata_json
                    .as_deref()
                    .is_some_and(|m| m.contains(&needle))
            })
            .map(|s| s.stripe_checkout_session_id)
            .collect()
    })
}

#[derive(SpacetimeType)]
pub struct DemoStripeResponse {
    pub status: u16,
    pub body: String,
}

/// Admins only: the `stripe_request` host helper (a relative `/v1/` path with the stored key).
#[spacetimedb::procedure]
pub fn demo_stripe_request(
    ctx: &mut ProcedureContext,
    method: String,
    path: String,
    form_body: Option<String>,
    idempotency_key: Option<String>,
) -> Result<DemoStripeResponse, String> {
    let sender = ctx.sender();
    if !ctx.with_tx(|tx| stripe::is_admin(tx, sender)) {
        return Err(stripe::errors::NOT_AUTHORIZED.into());
    }
    let r = stripe::stripe_request(
        ctx,
        &method,
        &path,
        form_body.as_deref(),
        idempotency_key.as_deref(),
    )?;
    Ok(DemoStripeResponse {
        status: r.status,
        body: r.body,
    })
}

/// Test hook: run the retention sweep as of a given time (the scheduled reducer rejects clients).
#[spacetimedb::reducer]
pub fn demo_prune_webhook_events_at(ctx: &ReducerContext, now_micros: i64) -> Result<(), String> {
    if !stripe::is_admin(ctx, ctx.sender()) {
        return Err(stripe::errors::NOT_AUTHORIZED.into());
    }
    stripe::prune_webhook_events(ctx, Timestamp::from_micros_since_unix_epoch(now_micros));
    Ok(())
}
