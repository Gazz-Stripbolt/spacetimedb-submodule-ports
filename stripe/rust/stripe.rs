//! # stripe.rs: Stripe payments, Checkout, subscriptions and webhooks, for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/stripe` (SpacetimeDB PR #5906, commit
//! `16396207cea1a07f4ac476864a05ffcfd19bfc25`, package `spacetime-stripe-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Mirrors customers, subscriptions, Checkout sessions, invoices and payments into private
//! tables from signed Stripe webhooks, with the same out-of-order protection, admin-gated
//! billing procedures, host helpers, error codes and limits as the TS package. Needs
//! `crypto.rs` from this repo (Stripe signatures) and `serde_json` with `preserve_order`.
//!
//! ```ignore
//! #[path = "crypto.rs"] pub mod crypto;
//! #[path = "stripe.rs"] pub mod stripe;
//!
//! #[spacetimedb::reducer(init)]
//! pub fn init(ctx: &ReducerContext) { stripe::install(ctx); }
//!
//! #[spacetimedb::http::router]
//! fn router() -> Router { Router::new().post("/stripe/webhook", stripe::stripe_webhook) }
//!
//! // A host procedure: resolve the user from ctx.sender(), take prices from your own catalog.
//! let session = stripe::create_user_checkout_session(ctx, stripe::UserCheckoutArgs { user_id, items, mode: "payment".into(), success_url, cancel_url, ..Default::default() })?;
//! ```
//!
//! Changes from the TypeScript original:
//! - Names: a drop-in shares your module's namespace, so generic procedures and reducers get a
//!   `stripe_` prefix (`get_customer` → `stripe_get_customer`, `replay_webhook_event` →
//!   `stripe_replay_webhook_event`, `prune_webhook_events` → `stripe_prune_webhook_events`, …)
//!   and generic types a `Stripe` prefix (`WebhookEventStatus` → `StripeWebhookEventStatus`,
//!   `CheckoutSessionResult` → `StripeCheckoutSessionResult`, `CheckoutLineItem` →
//!   `StripeCheckoutLineItem`, …). Names already carrying "stripe" are unchanged.
//! - Procedures return `Result<T, String>` instead of throwing.
//! - Lookups by user, org, or an optional customer/invoice id scan the table: Rust can't filter
//!   an index on an `Option` column (the indexes exist, for schema parity). Org and user
//!   subscription lists are sorted by `inserted_at`, like the TS composite indexes.
//! - `stripe_prune_webhook_events` rejects calls that don't come from the scheduler.
//! - Payload validation is hand-written (no valibot): `Failed` rows carry differently worded
//!   messages. A non-integer or out-of-range number where TS calls `BigInt()` aborts the
//!   transaction, as the TS `RangeError` does.
//! - The API origin can be overridden at build time with `STRIPE_API_BASE` (tests use a mock).

use crate::crypto;
use serde_json::{Map, Value};
use spacetimedb::http::{Body, HandlerContext, Request, Response, handler};
use spacetimedb::{
    Identity, ProcedureContext, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration,
    Timestamp,
};

/// Where API calls go. Override at build time with `STRIPE_API_BASE`.
pub const STRIPE_API_ORIGIN: &str = match option_env!("STRIPE_API_BASE") {
    Some(base) => base,
    None => "https://api.stripe.com",
};
pub const MAX_WEBHOOK_BODY_LENGTH: usize = 1024 * 1024;
pub const MAX_WEBHOOK_HEADER_LENGTH: usize = 8192;
pub const MAX_WEBHOOK_METADATA_LENGTH: usize = 255;
/// Stripe retries for up to three days; events are kept well past that.
pub const WEBHOOK_EVENT_RETENTION_MICROS: i64 = 30 * 24 * 60 * 60 * 1_000_000;
pub const WEBHOOK_PRUNE_INTERVAL_MICROS: i64 = 60 * 60 * 1_000_000;
pub const WEBHOOK_PRUNE_BATCH: usize = 500;
const MAX_PATH_LENGTH: usize = 2048;
const MAX_FORM_BODY_LENGTH: usize = 64 * 1024;
const MAX_IDEMPOTENCY_KEY_LENGTH: usize = 255;
const MAX_QUERY_ROWS: usize = 1000;

/// Stable error codes. Stripe API failures append `:<status>` and Stripe's error details.
pub mod errors {
    pub const NOT_AUTHORIZED: &str = "stripe.not_authorized";
    pub const CANNOT_REMOVE_LAST_ADMIN: &str = "stripe.cannot_remove_last_admin";
    pub const CONFIG_NOT_SET: &str = "stripe.config_not_set";
    pub const INVALID_WEBHOOK_SIGNING_SECRET: &str = "stripe.invalid_webhook_signing_secret";
    pub const REQUEST_INVALID: &str = "stripe.request_invalid";
    pub const REQUEST_PATH_INVALID: &str = "stripe.request_path_invalid";
    pub const REQUEST_METHOD_INVALID: &str = "stripe.request_method_invalid";
    pub const REQUEST_BODY_TOO_LARGE: &str = "stripe.request_body_too_large";
    pub const IDEMPOTENCY_KEY_TOO_LONG: &str = "stripe.idempotency_key_too_long";
    pub const CREATE_CUSTOMER_FAILED: &str = "stripe.create_customer_failed";
    pub const CREATE_CUSTOMER_INVALID_RESPONSE: &str = "stripe.create_customer_invalid_response";
    pub const CHECKOUT_SESSION_REQUIRES_ITEMS: &str = "stripe.checkout_session_requires_items";
    pub const CHECKOUT_SESSION_FAILED: &str = "stripe.checkout_session_failed";
    pub const CHECKOUT_SESSION_INVALID_RESPONSE: &str = "stripe.checkout_session_invalid_response";
    pub const PORTAL_SESSION_FAILED: &str = "stripe.portal_session_failed";
    pub const PORTAL_SESSION_INVALID_RESPONSE: &str = "stripe.portal_session_invalid_response";
    pub const SUBSCRIPTION_NOT_FOUND: &str = "stripe.subscription_not_found";
    pub const SUBSCRIPTION_UPDATE_FAILED: &str = "stripe.subscription_update_failed";
    pub const SUBSCRIPTION_UPDATE_INVALID_RESPONSE: &str =
        "stripe.subscription_update_invalid_response";
    pub const SUBSCRIPTION_CANCEL_FAILED: &str = "stripe.subscription_cancel_failed";
    pub const SUBSCRIPTION_CANCEL_INVALID_RESPONSE: &str =
        "stripe.subscription_cancel_invalid_response";
    pub const SUBSCRIPTION_LOOKUP_FAILED: &str = "stripe.subscription_lookup_failed";
    pub const SUBSCRIPTION_LOOKUP_INVALID_RESPONSE: &str =
        "stripe.subscription_lookup_invalid_response";
    pub const SUBSCRIPTION_MISSING_LINE_ITEMS: &str = "stripe.subscription_missing_line_items";
    pub const SUBSCRIPTION_ITEM_UPDATE_FAILED: &str = "stripe.subscription_item_update_failed";
    pub const SUBSCRIPTION_PAYLOAD_MISSING_FIELDS: &str =
        "stripe.subscription_payload_missing_fields";
    pub const WEBHOOK_METADATA_INVALID: &str = "stripe.webhook_metadata_invalid";
    pub const WEBHOOK_PAYLOAD_TOO_LARGE: &str = "stripe.webhook_payload_too_large";
    pub const WEBHOOK_SIGNATURE_TOO_LARGE: &str = "stripe.webhook_signature_too_large";
    pub const WEBHOOK_SECRET_NOT_CONFIGURED: &str = "stripe.webhook_secret_not_configured";
    pub const WEBHOOK_SIGNATURE_MISMATCH: &str = "stripe.webhook_signature_mismatch";
    pub const WEBHOOK_PAYLOAD_MISSING_METADATA: &str = "stripe.webhook_payload_missing_metadata";
    pub const WEBHOOK_METADATA_MISMATCH: &str = "stripe.webhook_metadata_mismatch";
    pub const WEBHOOK_EVENT_NOT_FOUND: &str = "stripe.webhook_event_not_found";
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Received = stored, Processed = applied, Ignored = unhandled type or older than the stored
/// state, Failed = a handled type whose payload couldn't be applied.
#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripeWebhookEventStatus {
    Received,
    Processed,
    Ignored,
    Failed,
}
use StripeWebhookEventStatus as S;

#[spacetimedb::table(accessor = stripe_customer)]
#[derive(Clone, Debug)]
pub struct StripeCustomer {
    #[primary_key]
    pub stripe_customer_id: String,
    pub app_user_id: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub metadata_json: Option<String>,
    #[index(btree)]
    pub user_id: Option<String>,
    pub event_created_unix: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(
    accessor = stripe_subscription,
    index(accessor = by_org_inserted_at, btree(columns = [org_id, inserted_at])),
    index(accessor = by_user_inserted_at, btree(columns = [user_id, inserted_at]))
)]
#[derive(Clone, Debug)]
pub struct StripeSubscription {
    #[primary_key]
    pub stripe_subscription_id: String,
    #[index(btree)]
    pub stripe_customer_id: String,
    pub status: String,
    pub current_period_end_unix: i64,
    pub cancel_at_period_end: bool,
    pub cancel_at_unix: Option<i64>,
    pub quantity: Option<i64>,
    pub price_id: Option<String>,
    pub metadata_json: Option<String>,
    #[index(btree)]
    pub org_id: Option<String>,
    #[index(btree)]
    pub user_id: Option<String>,
    pub event_created_unix: i64,
    pub inserted_at: Timestamp,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = stripe_checkout_session)]
#[derive(Clone, Debug)]
pub struct StripeCheckoutSession {
    #[primary_key]
    pub stripe_checkout_session_id: String,
    #[index(btree)]
    pub stripe_customer_id: Option<String>,
    pub status: String,
    pub payment_status: String,
    pub mode: String,
    pub metadata_json: Option<String>,
    pub event_created_unix: i64,
    pub inserted_at: Timestamp,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = stripe_payment)]
#[derive(Clone, Debug)]
pub struct StripePayment {
    #[primary_key]
    pub stripe_payment_intent_id: String,
    #[index(btree)]
    pub stripe_customer_id: Option<String>,
    #[index(btree)]
    pub stripe_invoice_id: Option<String>,
    pub amount: i64,
    pub currency: String,
    pub status: String,
    pub created_unix: i64,
    pub metadata_json: Option<String>,
    #[index(btree)]
    pub org_id: Option<String>,
    #[index(btree)]
    pub user_id: Option<String>,
    pub inserted_at: Timestamp,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = stripe_invoice)]
#[derive(Clone, Debug)]
pub struct StripeInvoice {
    #[primary_key]
    pub stripe_invoice_id: String,
    #[index(btree)]
    pub stripe_customer_id: String,
    #[index(btree)]
    pub stripe_subscription_id: Option<String>,
    pub status: String,
    pub amount_due: i64,
    pub amount_paid: i64,
    pub created_unix: i64,
    #[index(btree)]
    pub org_id: Option<String>,
    #[index(btree)]
    pub user_id: Option<String>,
    pub event_created_unix: i64,
    pub inserted_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Idempotency log with each event's payload (not its signature header).
#[spacetimedb::table(accessor = stripe_webhook_event)]
#[derive(Clone, Debug)]
pub struct StripeWebhookEvent {
    #[primary_key]
    pub event_id: String,
    pub event_type: String,
    pub livemode: bool,
    pub payload_json: String,
    pub status: StripeWebhookEventStatus,
    pub error_message: Option<String>,
    #[index(btree)]
    pub received_at: Timestamp,
    pub processed_at: Option<Timestamp>,
}

/// Schedules the retention sweep of `stripe_webhook_event`.
#[spacetimedb::table(accessor = stripe_webhook_prune_tick, scheduled(stripe_prune_webhook_events))]
pub struct StripeWebhookPruneTick {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

/// Private singleton holding the secrets.
#[spacetimedb::table(accessor = stripe_config)]
#[derive(Clone)]
pub struct StripeConfig {
    #[primary_key]
    pub singleton: bool,
    pub secret_key: String,
    pub stripe_version: Option<String>,
    pub webhook_signing_secret: Option<String>,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = stripe_admin_identity)]
pub struct StripeAdminIdentity {
    #[primary_key]
    pub identity: Identity,
    pub added_at: Timestamp,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct StripeCheckoutSessionResult {
    pub session_id: String,
    pub url: Option<String>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct StripeGetOrCreateCustomerResult {
    pub customer_id: String,
    pub is_new: bool,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct StripePortalSessionResult {
    pub url: String,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct StripeConfigStatus {
    pub is_configured: bool,
    pub has_webhook_secret: bool,
    pub stripe_version: Option<String>,
    pub secret_key_length: u16,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ValidateStripePriceResult {
    pub valid: bool,
    pub status: u16,
    pub active: Option<bool>,
    pub currency: Option<String>,
    pub unit_amount: Option<i64>,
    pub livemode: Option<bool>,
    /// TS `type`: Rust `SpacetimeType` can't name a field `r#type` (see FINDINGS).
    pub price_type: Option<String>,
    pub message: Option<String>,
    pub code: Option<String>,
    pub error_type: Option<String>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct StripeRemoteCheckoutSessionResult {
    pub ok: bool,
    pub status: u16,
    pub session_id: Option<String>,
    pub payment_status: Option<String>,
    pub session_status: Option<String>,
    pub mode: Option<String>,
    pub amount_total: Option<i64>,
    pub currency: Option<String>,
    pub customer_id: Option<String>,
    pub payment_intent_id: Option<String>,
    pub message: Option<String>,
    pub code: Option<String>,
    pub error_type: Option<String>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct StripeCheckoutLineItem {
    pub price_id: String,
    pub quantity: i64,
}

/// A Stripe API response.
#[derive(Clone, Debug)]
pub struct StripeResponse {
    pub status: u16,
    pub body: String,
}

// ---------------------------------------------------------------------------
// JS-compatible JSON and form helpers
// ---------------------------------------------------------------------------

/// Length in UTF-16 code units, like JS `string.length`.
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn js_slice(s: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        out.push(c);
    }
    out
}

fn has_control_character(s: &str) -> bool {
    s.chars().any(|c| (c as u32) <= 0x1f || c as u32 == 0x7f)
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() || c == '\u{feff}' {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// JS orders "array index" keys (canonical integers below 2^32 - 1) first, ascending.
fn is_array_index(k: &str) -> bool {
    !k.is_empty()
        && k.bytes().all(|b| b.is_ascii_digit())
        && (k == "0" || !k.starts_with('0'))
        && k.parse::<u64>().is_ok_and(|n| n < 4_294_967_295)
}

fn js_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{f:.0}")
    } else {
        format!("{f}")
    }
}

/// `JSON.stringify(JSON.parse(…))` as JS would write it: integer-like keys first, `1.0` as `1`.
pub fn js_json(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number(n),
        Value::String(s) => serde_json::to_string(s).expect("string serializes"),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(js_json).collect::<Vec<_>>().join(",")
        ),
        Value::Object(m) => {
            let parts: Vec<String> = js_entries(m)
                .into_iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).expect("key serializes"),
                        js_json(v)
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// `Object.entries` order.
fn js_entries(m: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut index: Vec<(u64, &String, &Value)> = Vec::new();
    let mut rest = Vec::new();
    for (k, v) in m {
        if is_array_index(k) {
            index.push((k.parse().unwrap_or(0), k, v));
        } else {
            rest.push((k, v));
        }
    }
    index.sort_by_key(|(n, _, _)| *n);
    index
        .into_iter()
        .map(|(_, k, v)| (k, v))
        .chain(rest)
        .collect()
}

/// JS `String(value)`.
fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number(n),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| {
                if i.is_null() {
                    String::new()
                } else {
                    js_string(i)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// JS `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `key=value&…`, skipping `None` values.
pub fn form_pairs_to_body(pairs: &[(String, Option<String>)]) -> String {
    pairs
        .iter()
        .filter_map(|(k, v)| {
            v.as_ref()
                .map(|v| format!("{}={}", encode_uri_component(k), encode_uri_component(v)))
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// `prefix[key]=String(value)` for each non-null entry of a JSON object (or array).
fn metadata_json_to_form_pairs(
    prefix: &str,
    metadata_json: Option<&str>,
) -> Vec<(String, Option<String>)> {
    let Some(Ok(parsed)) = metadata_json
        .filter(|s| !s.is_empty())
        .map(serde_json::from_str::<Value>)
    else {
        return Vec::new();
    };
    let entries: Vec<(String, Value)> = match &parsed {
        Value::Object(m) => js_entries(m)
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        Value::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        _ => return Vec::new(),
    };
    entries
        .into_iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (format!("{prefix}[{k}]"), Some(js_string(&v))))
        .collect()
}

fn is_record(v: &Value) -> bool {
    v.is_object() || v.is_array()
}

fn maybe_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

fn maybe_bool(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}

/// `maybeBigIntFromUnknown`: a finite integer number, else `None`.
fn maybe_i64(v: Option<&Value>) -> Option<i64> {
    let n = v?.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    (f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18).then_some(f as i64)
}

/// `maybeId`: a string, or an object with a string `id`.
fn maybe_id(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => maybe_string(o.get("id")),
        _ => None,
    }
}

/// `metadataInfo`: JSON text plus `orgId` / `userId`, for any object or array.
fn metadata_info(metadata: Option<&Value>) -> (Option<String>, Option<String>, Option<String>) {
    match metadata {
        Some(v @ Value::Object(o)) => (
            Some(js_json(v)),
            maybe_string(o.get("orgId")),
            maybe_string(o.get("userId")),
        ),
        Some(v @ Value::Array(_)) => (Some(js_json(v)), None, None),
        _ => (None, None, None),
    }
}

/// `coerceMetadataFromJson`.
fn coerce_metadata_from_json(
    metadata_json: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>) {
    let Some(raw) = metadata_json.filter(|s| !s.is_empty()) else {
        return (None, None, None);
    };
    let parsed = serde_json::from_str::<Value>(raw).ok();
    let (json, org, user) = metadata_info(parsed.as_ref());
    (Some(json.unwrap_or_else(|| raw.to_string())), org, user)
}

fn derive_cancel_at_period_end(cancel_at_unix: Option<i64>, current_period_end_unix: i64) -> bool {
    match cancel_at_unix {
        Some(c) if current_period_end_unix > 0 => (c - current_period_end_unix).abs() <= 5 * 60,
        _ => false,
    }
}

/// `:type=…|code=…|msg=…|log=…` from a Stripe error body, else `:body=…`.
pub fn stripe_error_suffix(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<Value>(body)
        && is_record(&parsed)
        && let Some(e) = parsed.get("error").filter(|e| is_record(e))
    {
        let mut parts = Vec::new();
        if let Some(t) = maybe_string(e.get("type")).filter(|s| !s.is_empty()) {
            parts.push(format!("type={t}"));
        }
        if let Some(c) = maybe_string(e.get("code")).filter(|s| !s.is_empty()) {
            parts.push(format!("code={c}"));
        }
        if let Some(m) = maybe_string(e.get("message")).filter(|s| !s.is_empty()) {
            parts.push(format!("msg={}", js_slice(&collapse_whitespace(&m), 240)));
        }
        if let Some(l) = maybe_string(e.get("request_log_url")).filter(|s| !s.is_empty()) {
            parts.push(format!("log={l}"));
        }
        if !parts.is_empty() {
            return format!(":{}", parts.join("|"));
        }
    }
    let compact = collapse_whitespace(body);
    let compact = compact.trim();
    if compact.is_empty() {
        String::new()
    } else {
        format!(":body={}", js_slice(compact, 240))
    }
}

fn unix_seconds(t: Timestamp) -> i64 {
    t.to_micros_since_unix_epoch().div_euclid(1_000_000)
}

// ---------------------------------------------------------------------------
// Admins and config
// ---------------------------------------------------------------------------

/// Call from your `init` reducer: seeds the publishing identity as admin and schedules the
/// hourly webhook retention sweep.
pub fn install(ctx: &ReducerContext) {
    if ctx
        .db
        .stripe_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        ctx.db.stripe_admin_identity().insert(StripeAdminIdentity {
            identity: ctx.sender(),
            added_at: ctx.timestamp,
        });
    }
    if ctx.db.stripe_webhook_prune_tick().count() == 0 {
        ctx.db
            .stripe_webhook_prune_tick()
            .insert(StripeWebhookPruneTick {
                scheduled_id: 0,
                scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(
                    WEBHOOK_PRUNE_INTERVAL_MICROS,
                )),
            });
    }
}

pub fn is_admin(ctx: &ReducerContext, who: Identity) -> bool {
    ctx.db
        .stripe_admin_identity()
        .identity()
        .find(who)
        .is_some()
}

fn require_procedure_admin(ctx: &mut ProcedureContext) -> Result<(), String> {
    let sender = ctx.sender();
    if ctx.with_tx(|tx| is_admin(tx, sender)) {
        Ok(())
    } else {
        Err(errors::NOT_AUTHORIZED.into())
    }
}

#[spacetimedb::procedure]
pub fn stripe_add_admin_identity(
    ctx: &mut ProcedureContext,
    identity: Identity,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    let now = ctx.timestamp;
    ctx.with_tx(|tx| {
        if tx
            .db
            .stripe_admin_identity()
            .identity()
            .find(identity)
            .is_none()
        {
            tx.db.stripe_admin_identity().insert(StripeAdminIdentity {
                identity,
                added_at: now,
            });
        }
    });
    Ok(())
}

#[spacetimedb::procedure]
pub fn stripe_remove_admin_identity(
    ctx: &mut ProcedureContext,
    identity: Identity,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    ctx.try_with_tx(|tx| {
        if tx
            .db
            .stripe_admin_identity()
            .identity()
            .find(identity)
            .is_none()
        {
            return Ok(());
        }
        if tx.db.stripe_admin_identity().count() <= 1 {
            return Err(errors::CANNOT_REMOVE_LAST_ADMIN.to_string());
        }
        tx.db.stripe_admin_identity().identity().delete(identity);
        Ok(())
    })
}

pub fn load_config(ctx: &ReducerContext) -> Result<StripeConfig, String> {
    ctx.db
        .stripe_config()
        .singleton()
        .find(true)
        .ok_or_else(|| errors::CONFIG_NOT_SET.to_string())
}

/// Admins only. `None` keeps the stored version / webhook secret.
#[spacetimedb::procedure]
pub fn set_stripe_config(
    ctx: &mut ProcedureContext,
    secret_key: String,
    stripe_version: Option<String>,
    webhook_signing_secret: Option<String>,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    let now = ctx.timestamp;
    ctx.with_tx(|tx| {
        let existing = tx.db.stripe_config().singleton().find(true);
        let row = StripeConfig {
            singleton: true,
            secret_key: secret_key.clone(),
            stripe_version: stripe_version
                .clone()
                .or_else(|| existing.as_ref().and_then(|e| e.stripe_version.clone())),
            webhook_signing_secret: webhook_signing_secret.clone().or_else(|| {
                existing
                    .as_ref()
                    .and_then(|e| e.webhook_signing_secret.clone())
            }),
            updated_at: now,
        };
        if existing.is_some() {
            tx.db.stripe_config().singleton().update(row);
        } else {
            tx.db.stripe_config().insert(row);
        }
    });
    Ok(())
}

/// Admins only: rotate just the webhook secret.
#[spacetimedb::procedure]
pub fn set_stripe_webhook_signing_secret(
    ctx: &mut ProcedureContext,
    webhook_signing_secret: String,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    let secret = webhook_signing_secret.trim().to_string();
    if secret.is_empty() {
        return Err(errors::INVALID_WEBHOOK_SIGNING_SECRET.into());
    }
    let now = ctx.timestamp;
    ctx.try_with_tx(|tx| {
        let mut existing = load_config(tx)?;
        existing.webhook_signing_secret = Some(secret.clone());
        existing.updated_at = now;
        tx.db.stripe_config().singleton().update(existing);
        Ok(())
    })
}

#[spacetimedb::procedure]
pub fn get_stripe_config_status(ctx: &mut ProcedureContext) -> Result<StripeConfigStatus, String> {
    require_procedure_admin(ctx)?;
    Ok(
        ctx.with_tx(|tx| match tx.db.stripe_config().singleton().find(true) {
            None => StripeConfigStatus {
                is_configured: false,
                has_webhook_secret: false,
                stripe_version: None,
                secret_key_length: 0,
            },
            Some(row) => StripeConfigStatus {
                is_configured: true,
                has_webhook_secret: row.webhook_signing_secret.is_some(),
                stripe_version: row.stripe_version,
                secret_key_length: js_len(&row.secret_key).min(u16::MAX as usize) as u16,
            },
        }),
    )
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// A validated request (the TS `buildStripeHttpRequest`).
#[derive(Clone, Debug)]
pub struct StripeHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

pub fn build_stripe_http_request(
    method: &str,
    path: &str,
    secret_key: &str,
    stripe_version: Option<&str>,
    form_body: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<StripeHttpRequest, String> {
    let method = method.trim().to_uppercase();
    if !["GET", "POST", "DELETE"].contains(&method.as_str()) {
        return Err(errors::REQUEST_METHOD_INVALID.into());
    }
    let path = path.trim();
    if !path.starts_with("/v1/")
        || path.starts_with("//")
        || path.contains('\\')
        || path.contains('#')
        || js_len(path) > MAX_PATH_LENGTH
        || has_control_character(path)
    {
        return Err(errors::REQUEST_PATH_INVALID.into());
    }
    let body = form_body.filter(|b| !b.is_empty());
    if body.is_some_and(|b| js_len(b) > MAX_FORM_BODY_LENGTH) {
        return Err(errors::REQUEST_BODY_TOO_LARGE.into());
    }
    if idempotency_key.is_some_and(|k| js_len(k) > MAX_IDEMPOTENCY_KEY_LENGTH) {
        return Err(errors::IDEMPOTENCY_KEY_TOO_LONG.into());
    }
    let mut headers = vec![("Authorization".to_string(), format!("Bearer {secret_key}"))];
    if let Some(v) = stripe_version.filter(|v| !v.is_empty()) {
        headers.push(("Stripe-Version".into(), v.to_string()));
    }
    if let Some(k) = idempotency_key.filter(|k| !k.is_empty()) {
        headers.push(("Idempotency-Key".into(), k.to_string()));
    }
    if body.is_some() {
        headers.push((
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        ));
    }
    Ok(StripeHttpRequest {
        method,
        url: format!("{STRIPE_API_ORIGIN}{path}"),
        headers,
        body: body.map(str::to_string),
    })
}

fn call_stripe(
    ctx: &mut ProcedureContext,
    cfg: &StripeConfig,
    method: &str,
    path: &str,
    form_body: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<StripeResponse, String> {
    let r = build_stripe_http_request(
        method,
        path,
        &cfg.secret_key,
        cfg.stripe_version.as_deref(),
        form_body,
        idempotency_key,
    )?;
    let mut builder = spacetimedb::http::Request::builder()
        .method(r.method.as_str())
        .uri(&r.url);
    for (k, v) in &r.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let request = builder
        .body(r.body.unwrap_or_default())
        .map_err(|_| errors::REQUEST_INVALID.to_string())?;
    let response = ctx.http.send(request).map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    Ok(StripeResponse {
        status,
        body: response.into_body().into_string_lossy(),
    })
}

fn ensure_ok(r: &StripeResponse, code: &str) -> Result<(), String> {
    if (200..300).contains(&r.status) {
        Ok(())
    } else {
        Err(format!(
            "{code}:{}{}",
            r.status,
            stripe_error_suffix(&r.body)
        ))
    }
}

fn config(ctx: &mut ProcedureContext) -> Result<StripeConfig, String> {
    ctx.with_tx(|tx| load_config(tx))
}

/// A request to a relative `/v1/` path with the stored secret key. No authorization.
pub fn stripe_request(
    ctx: &mut ProcedureContext,
    method: &str,
    path: &str,
    form_body: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<StripeResponse, String> {
    let cfg = config(ctx)?;
    call_stripe(ctx, &cfg, method, path, form_body, idempotency_key)
}

// ---------------------------------------------------------------------------
// Row upserts (sparse: `None` keeps the stored value where TS uses `??`)
// ---------------------------------------------------------------------------

fn upsert_customer(ctx: &ReducerContext, now: Timestamp, mut row: StripeCustomer) {
    let existing = ctx
        .db
        .stripe_customer()
        .stripe_customer_id()
        .find(&row.stripe_customer_id);
    if let Some(e) = &existing {
        row.app_user_id = row.app_user_id.or_else(|| e.app_user_id.clone());
        row.email = row.email.or_else(|| e.email.clone());
        row.name = row.name.or_else(|| e.name.clone());
        row.metadata_json = row.metadata_json.or_else(|| e.metadata_json.clone());
        row.user_id = row.user_id.or_else(|| e.user_id.clone());
        row.created_at = e.created_at;
    } else {
        row.created_at = now;
    }
    row.updated_at = now;
    if existing.is_some() {
        ctx.db.stripe_customer().stripe_customer_id().update(row);
    } else {
        ctx.db.stripe_customer().insert(row);
    }
}

fn upsert_subscription(ctx: &ReducerContext, now: Timestamp, mut row: StripeSubscription) {
    let existing = ctx
        .db
        .stripe_subscription()
        .stripe_subscription_id()
        .find(&row.stripe_subscription_id);
    if let Some(e) = &existing {
        row.price_id = row.price_id.or_else(|| e.price_id.clone());
        row.metadata_json = row.metadata_json.or_else(|| e.metadata_json.clone());
        row.org_id = row.org_id.or_else(|| e.org_id.clone());
        row.user_id = row.user_id.or_else(|| e.user_id.clone());
        row.inserted_at = e.inserted_at;
    } else {
        row.inserted_at = now;
    }
    row.updated_at = now;
    if existing.is_some() {
        ctx.db
            .stripe_subscription()
            .stripe_subscription_id()
            .update(row);
    } else {
        ctx.db.stripe_subscription().insert(row);
    }
}

fn upsert_checkout_session(ctx: &ReducerContext, now: Timestamp, mut row: StripeCheckoutSession) {
    let existing = ctx
        .db
        .stripe_checkout_session()
        .stripe_checkout_session_id()
        .find(&row.stripe_checkout_session_id);
    if let Some(e) = &existing {
        row.stripe_customer_id = row
            .stripe_customer_id
            .or_else(|| e.stripe_customer_id.clone());
        row.metadata_json = row.metadata_json.or_else(|| e.metadata_json.clone());
        row.inserted_at = e.inserted_at;
    } else {
        row.inserted_at = now;
    }
    row.updated_at = now;
    if existing.is_some() {
        ctx.db
            .stripe_checkout_session()
            .stripe_checkout_session_id()
            .update(row);
    } else {
        ctx.db.stripe_checkout_session().insert(row);
    }
}

fn upsert_payment(ctx: &ReducerContext, now: Timestamp, mut row: StripePayment) {
    let existing = ctx
        .db
        .stripe_payment()
        .stripe_payment_intent_id()
        .find(&row.stripe_payment_intent_id);
    if let Some(e) = &existing {
        row.stripe_customer_id = row
            .stripe_customer_id
            .or_else(|| e.stripe_customer_id.clone());
        row.stripe_invoice_id = row
            .stripe_invoice_id
            .or_else(|| e.stripe_invoice_id.clone());
        row.metadata_json = row.metadata_json.or_else(|| e.metadata_json.clone());
        row.org_id = row.org_id.or_else(|| e.org_id.clone());
        row.user_id = row.user_id.or_else(|| e.user_id.clone());
        row.inserted_at = e.inserted_at;
    } else {
        row.inserted_at = now;
    }
    row.updated_at = now;
    if existing.is_some() {
        ctx.db
            .stripe_payment()
            .stripe_payment_intent_id()
            .update(row);
    } else {
        ctx.db.stripe_payment().insert(row);
    }
}

fn upsert_invoice(ctx: &ReducerContext, now: Timestamp, mut row: StripeInvoice) {
    let existing = ctx
        .db
        .stripe_invoice()
        .stripe_invoice_id()
        .find(&row.stripe_invoice_id);
    if let Some(e) = &existing {
        row.stripe_subscription_id = row
            .stripe_subscription_id
            .or_else(|| e.stripe_subscription_id.clone());
        row.org_id = row.org_id.or_else(|| e.org_id.clone());
        row.user_id = row.user_id.or_else(|| e.user_id.clone());
        row.inserted_at = e.inserted_at;
    } else {
        row.inserted_at = now;
    }
    row.updated_at = now;
    if existing.is_some() {
        ctx.db.stripe_invoice().stripe_invoice_id().update(row);
    } else {
        ctx.db.stripe_invoice().insert(row);
    }
}

fn update_webhook_status(ctx: &ReducerContext, event_id: &str, status: S, error: Option<String>) {
    let Some(mut row) = ctx
        .db
        .stripe_webhook_event()
        .event_id()
        .find(event_id.to_string())
    else {
        return;
    };
    if matches!(status, S::Processed | S::Ignored | S::Failed) {
        row.processed_at = Some(ctx.timestamp);
    }
    row.status = status;
    row.error_message = error;
    ctx.db.stripe_webhook_event().event_id().update(row);
}

// ---------------------------------------------------------------------------
// Out-of-order protection
// ---------------------------------------------------------------------------

/// An update is stale when older than the stored state or when it moves the status backward.
pub fn is_stale(existing: Option<(i64, u8)>, next: (i64, u8)) -> bool {
    existing.is_some_and(|(created, rank)| next.0 < created || next.1 < rank)
}

pub fn subscription_status_rank(status: &str) -> u8 {
    match status {
        "incomplete" => 0,
        "canceled" | "incomplete_expired" => 2,
        _ => 1,
    }
}

pub fn invoice_status_rank(status: &str) -> u8 {
    match status {
        "draft" => 0,
        "uncollectible" => 2,
        "paid" | "void" => 3,
        _ => 1,
    }
}

pub fn checkout_payment_status_rank(payment_status: &str) -> u8 {
    match payment_status {
        "unpaid" => 0,
        "failed" => 1,
        _ => 2,
    }
}

// ---------------------------------------------------------------------------
// Event validation (the TS valibot schemas)
// ---------------------------------------------------------------------------

const HANDLED_EVENT_TYPES: [&str; 15] = [
    "customer.created",
    "customer.updated",
    "customer.subscription.created",
    "customer.subscription.updated",
    "customer.subscription.deleted",
    "checkout.session.completed",
    "checkout.session.async_payment_succeeded",
    "checkout.session.async_payment_failed",
    "invoice.created",
    "invoice.finalized",
    "invoice.paid",
    "invoice.payment_succeeded",
    "invoice.payment_failed",
    "invoice_payment.paid",
    "payment_intent.succeeded",
];

type Obj = Map<String, Value>;

/// A JS number (`f64` semantics); `BigInt()` of it may abort.
#[derive(Clone, Copy, Debug)]
struct Num(f64, Option<i64>);

impl Num {
    fn from(v: &Value) -> Option<Num> {
        let n = v.as_number()?;
        Some(Num(n.as_f64().unwrap_or(0.0), n.as_i64()))
    }
    /// JS `BigInt(n)` then serialized as i64: non-integers and out-of-range values abort.
    fn big(self) -> Result<i64, String> {
        if let Some(i) = self.1 {
            return Ok(i);
        }
        let f = self.0;
        if f.is_finite()
            && f.fract() == 0.0
            && (-9.223_372_036_854_775e18..9.223_372_036_854_775e18).contains(&f)
        {
            Ok(f as i64)
        } else {
            Err(format!(
                "RangeError: The number {f} cannot be converted to a BigInt or i64"
            ))
        }
    }
}

fn issue_missing(path: &str) -> String {
    format!("Invalid key: Expected property but received undefined at {path}")
}
fn issue_type(expected: &str, path: &str) -> String {
    format!("Invalid type: Expected {expected} at {path}")
}

fn req_str(o: &Obj, k: &str, path: &str) -> Result<String, String> {
    match o.get(k) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(issue_type("string", &format!("{path}{k}"))),
        None => Err(issue_missing(&format!("{path}{k}"))),
    }
}
/// `v.optional(v.string())`.
fn opt_str(o: &Obj, k: &str, path: &str) -> Result<Option<String>, String> {
    match o.get(k) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(issue_type("string", &format!("{path}{k}"))),
    }
}
/// `v.optional(v.union([v.string(), v.null()]))`.
fn opt_str_or_null(o: &Obj, k: &str, path: &str) -> Result<Option<String>, String> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(issue_type("string | null", &format!("{path}{k}"))),
    }
}
fn req_num(o: &Obj, k: &str, path: &str) -> Result<Num, String> {
    match o.get(k) {
        Some(v) => Num::from(v).ok_or_else(|| issue_type("number", &format!("{path}{k}"))),
        None => Err(issue_missing(&format!("{path}{k}"))),
    }
}
fn opt_num(o: &Obj, k: &str, path: &str) -> Result<Option<Num>, String> {
    match o.get(k) {
        None => Ok(None),
        Some(v) => Num::from(v)
            .map(Some)
            .ok_or_else(|| issue_type("number", &format!("{path}{k}"))),
    }
}
fn opt_num_or_null(o: &Obj, k: &str, path: &str) -> Result<Option<Num>, String> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => Num::from(v)
            .map(Some)
            .ok_or_else(|| issue_type("number | null", &format!("{path}{k}"))),
    }
}
fn opt_bool(o: &Obj, k: &str, path: &str) -> Result<Option<bool>, String> {
    match o.get(k) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(issue_type("boolean", &format!("{path}{k}"))),
    }
}
fn req_obj<'a>(o: &'a Obj, k: &str, path: &str) -> Result<&'a Obj, String> {
    match o.get(k) {
        Some(Value::Object(m)) => Ok(m),
        Some(_) => Err(issue_type("Object", &format!("{path}{k}"))),
        None => Err(issue_missing(&format!("{path}{k}"))),
    }
}
/// Expandable: a string id or `{ id: string }`.
fn expandable(v: &Value, path: &str) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Object(m) => req_str(m, "id", &format!("{path}.")),
        _ => Err(issue_type("string | Object", path)),
    }
}
fn req_expandable(o: &Obj, k: &str, path: &str) -> Result<String, String> {
    let p = format!("{path}{k}");
    expandable(o.get(k).ok_or_else(|| issue_missing(&p))?, &p)
}
/// `v.optional(vExpandableId)`.
fn opt_expandable(o: &Obj, k: &str, path: &str) -> Result<Option<String>, String> {
    match o.get(k) {
        None => Ok(None),
        Some(v) => expandable(v, &format!("{path}{k}")).map(Some),
    }
}
/// Expandable or null, required key: `Some(None)` is null.
fn req_expandable_or_null(o: &Obj, k: &str, path: &str) -> Result<Option<String>, String> {
    match o.get(k) {
        None => Err(issue_missing(&format!("{path}{k}"))),
        Some(Value::Null) => Ok(None),
        Some(v) => expandable(v, &format!("{path}{k}")).map(Some),
    }
}
/// Optional expandable-or-null: outer `None` = missing, `Some(None)` = null.
fn opt_expandable_or_null(o: &Obj, k: &str, path: &str) -> Result<Option<Option<String>>, String> {
    match o.get(k) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(v) => expandable(v, &format!("{path}{k}")).map(|s| Some(Some(s))),
    }
}
/// `vMetadata`: optional record of strings, or null.
fn metadata(o: &Obj, path: &str) -> Result<Option<Obj>, String> {
    match o.get("metadata") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => {
            for (k, v) in m {
                if !v.is_string() {
                    return Err(issue_type("string", &format!("{path}metadata.{k}")));
                }
            }
            Ok(Some(m.clone()))
        }
        Some(_) => Err(issue_type("Object | null", &format!("{path}metadata"))),
    }
}

/// `metadataInfoFromRecord`.
fn record_info(m: &Option<Obj>) -> (Option<String>, Option<String>, Option<String>) {
    match m {
        None => (None, None, None),
        Some(m) => (
            Some(js_json(&Value::Object(m.clone()))),
            maybe_string(m.get("orgId")),
            maybe_string(m.get("userId")),
        ),
    }
}

struct SubItem {
    current_period_end: Option<Num>,
    quantity: Option<Num>,
    price_id: Option<String>,
}

struct InvoiceObj {
    id: String,
    customer: Option<String>,
    subscription: Option<Option<String>>,
    parent_subscription: Option<String>,
    parent_metadata: Option<Obj>,
    status: Option<String>,
    amount_due: Option<Num>,
    amount_paid: Option<Num>,
    created: Option<Num>,
}

enum Event {
    Customer {
        id: String,
        email: Option<String>,
        name: Option<String>,
        metadata: Option<Obj>,
    },
    Subscription {
        deleted: bool,
        id: String,
        customer: String,
        status: String,
        current_period_end: Option<Num>,
        cancel_at: Option<Num>,
        cancel_at_period_end: Option<bool>,
        first_item: Option<SubItem>,
        metadata: Option<Obj>,
    },
    Checkout {
        failed: bool,
        id: String,
        status: Option<String>,
        payment_status: String,
        mode: Option<String>,
        customer: Option<Option<String>>,
        metadata: Option<Obj>,
    },
    Invoice {
        default_status: &'static str,
        obj: InvoiceObj,
    },
    InvoicePayment {
        invoice: String,
        amount_paid: Option<Num>,
        currency: String,
        created: Num,
        payment_intent: Option<String>,
    },
    PaymentIntent {
        id: String,
        customer: Option<Option<String>>,
        invoice: Option<Option<String>>,
        amount: Option<Num>,
        currency: Option<String>,
        status: Option<String>,
        created: Option<Num>,
        metadata: Option<Obj>,
    },
}

fn parse_event(v: &Value) -> Result<(Event, Num), String> {
    let Value::Object(root) = v else {
        return Err(issue_type("Object", ""));
    };
    let t = match root.get("type") {
        Some(Value::String(t)) if HANDLED_EVENT_TYPES.contains(&t.as_str()) => t.as_str(),
        _ => return Err("Invalid type: Expected a handled event type at type".into()),
    };
    let created = req_num(root, "created", "")?;
    let data = req_obj(root, "data", "")?;
    let o = req_obj(data, "object", "data.")?;
    let p = "data.object.";
    let ev = match t {
        "customer.created" | "customer.updated" => Event::Customer {
            id: req_str(o, "id", p)?,
            email: opt_str_or_null(o, "email", p)?,
            name: opt_str_or_null(o, "name", p)?,
            metadata: metadata(o, p)?,
        },
        "customer.subscription.created"
        | "customer.subscription.updated"
        | "customer.subscription.deleted" => {
            let id = req_str(o, "id", p)?;
            let customer = req_expandable(o, "customer", p)?;
            let status = req_str(o, "status", p)?;
            let current_period_end = opt_num(o, "current_period_end", p)?;
            let cancel_at = opt_num_or_null(o, "cancel_at", p)?;
            let cancel_at_period_end = opt_bool(o, "cancel_at_period_end", p)?;
            let mut items = Vec::new();
            match o.get("items") {
                None => {}
                Some(Value::Object(it)) => match it.get("data") {
                    Some(Value::Array(list)) => {
                        for (i, item) in list.iter().enumerate() {
                            let ip = format!("{p}items.data.{i}.");
                            let Value::Object(item) = item else {
                                return Err(issue_type("Object", ip.trim_end_matches('.')));
                            };
                            let price_id = match item.get("price") {
                                None | Some(Value::Null) => None,
                                Some(Value::Object(pr)) => {
                                    Some(req_str(pr, "id", &format!("{ip}price."))?)
                                }
                                Some(_) => {
                                    return Err(issue_type("Object | null", &format!("{ip}price")));
                                }
                            };
                            items.push(SubItem {
                                current_period_end: opt_num(item, "current_period_end", &ip)?,
                                quantity: opt_num(item, "quantity", &ip)?,
                                price_id,
                            });
                        }
                    }
                    Some(_) => return Err(issue_type("Array", &format!("{p}items.data"))),
                    None => return Err(issue_missing(&format!("{p}items.data"))),
                },
                Some(_) => return Err(issue_type("Object", &format!("{p}items"))),
            }
            Event::Subscription {
                deleted: t == "customer.subscription.deleted",
                id,
                customer,
                status,
                current_period_end,
                cancel_at,
                cancel_at_period_end,
                first_item: items.into_iter().next(),
                metadata: metadata(o, p)?,
            }
        }
        "checkout.session.completed"
        | "checkout.session.async_payment_succeeded"
        | "checkout.session.async_payment_failed" => Event::Checkout {
            failed: t == "checkout.session.async_payment_failed",
            id: req_str(o, "id", p)?,
            status: opt_str_or_null(o, "status", p)?,
            payment_status: req_str(o, "payment_status", p)?,
            mode: opt_str(o, "mode", p)?,
            customer: opt_expandable_or_null(o, "customer", p)?,
            metadata: metadata(o, p)?,
        },
        "invoice.created"
        | "invoice.finalized"
        | "invoice.paid"
        | "invoice.payment_succeeded"
        | "invoice.payment_failed" => {
            let id = req_str(o, "id", p)?;
            let customer = req_expandable_or_null(o, "customer", p)?;
            let subscription = opt_expandable_or_null(o, "subscription", p)?;
            let (mut parent_subscription, mut parent_metadata) = (None, None);
            match o.get("parent") {
                None | Some(Value::Null) => {}
                Some(Value::Object(parent)) => match parent.get("subscription_details") {
                    None | Some(Value::Null) => {}
                    Some(Value::Object(sd)) => {
                        let sp = format!("{p}parent.subscription_details.");
                        parent_subscription = Some(req_expandable(sd, "subscription", &sp)?);
                        parent_metadata = metadata(sd, &sp)?;
                    }
                    Some(_) => {
                        return Err(issue_type(
                            "Object | null",
                            &format!("{p}parent.subscription_details"),
                        ));
                    }
                },
                Some(_) => return Err(issue_type("Object | null", &format!("{p}parent"))),
            }
            Event::Invoice {
                default_status: match t {
                    "invoice.created" => "draft",
                    "invoice.finalized" | "invoice.payment_failed" => "open",
                    _ => "paid",
                },
                obj: InvoiceObj {
                    id,
                    customer,
                    subscription,
                    parent_subscription,
                    parent_metadata,
                    status: opt_str_or_null(o, "status", p)?,
                    amount_due: opt_num(o, "amount_due", p)?,
                    amount_paid: opt_num(o, "amount_paid", p)?,
                    created: opt_num(o, "created", p)?,
                },
            }
        }
        "invoice_payment.paid" => {
            let invoice = req_expandable(o, "invoice", p)?;
            let amount_paid = match o.get("amount_paid") {
                None => return Err(issue_missing(&format!("{p}amount_paid"))),
                Some(Value::Null) => None,
                Some(v) => Some(
                    Num::from(v)
                        .ok_or_else(|| issue_type("number | null", &format!("{p}amount_paid")))?,
                ),
            };
            let currency = req_str(o, "currency", p)?;
            let created = req_num(o, "created", p)?;
            let payment = req_obj(o, "payment", p)?;
            Event::InvoicePayment {
                invoice,
                amount_paid,
                currency,
                created,
                payment_intent: opt_expandable(payment, "payment_intent", &format!("{p}payment."))?,
            }
        }
        _ => Event::PaymentIntent {
            id: req_str(o, "id", p)?,
            customer: opt_expandable_or_null(o, "customer", p)?,
            invoice: opt_expandable_or_null(o, "invoice", p)?,
            amount: opt_num(o, "amount", p)?,
            currency: opt_str(o, "currency", p)?,
            status: opt_str(o, "status", p)?,
            created: opt_num(o, "created", p)?,
            metadata: metadata(o, p)?,
        },
    };
    Ok((ev, created))
}

fn big(n: Option<Num>) -> Result<Option<i64>, String> {
    n.map(Num::big).transpose()
}

// ---------------------------------------------------------------------------
// Applying events
// ---------------------------------------------------------------------------

/// Apply a payload. `Err` aborts the transaction (the TS code would throw).
pub fn apply_stripe_event(
    ctx: &ReducerContext,
    payload_json: &str,
) -> Result<(S, Option<String>), String> {
    let Ok(parsed) = serde_json::from_str::<Value>(payload_json) else {
        return Ok((S::Failed, Some("invalid JSON payload".into())));
    };
    match parse_event(&parsed) {
        Ok((event, created)) => Ok((dispatch_event(ctx, event, created.big()?)?, None)),
        Err(issue) => {
            let handled = parsed
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| HANDLED_EVENT_TYPES.contains(&t));
            if handled {
                Ok((
                    S::Failed,
                    Some(format!("payload validation failed: {issue}")),
                ))
            } else {
                Ok((S::Ignored, None))
            }
        }
    }
}

fn sync_invoice_event(
    ctx: &ReducerContext,
    obj: InvoiceObj,
    status: String,
    event_created_unix: i64,
) -> Result<S, String> {
    let existing = ctx.db.stripe_invoice().stripe_invoice_id().find(&obj.id);
    if is_stale(
        existing
            .as_ref()
            .map(|e| (e.event_created_unix, invoice_status_rank(&e.status))),
        (event_created_unix, invoice_status_rank(&status)),
    ) {
        return Ok(S::Ignored);
    }
    let Some(customer_id) = existing
        .as_ref()
        .map(|e| e.stripe_customer_id.clone())
        .or(obj.customer.clone())
    else {
        return Ok(S::Failed);
    };
    let legacy = obj.subscription.clone().flatten();
    let subscription_id = existing
        .as_ref()
        .and_then(|e| e.stripe_subscription_id.clone())
        .or(legacy.or(obj.parent_subscription.clone()));
    let subscription = subscription_id
        .as_ref()
        .filter(|s| !s.is_empty())
        .and_then(|s| {
            ctx.db
                .stripe_subscription()
                .stripe_subscription_id()
                .find(s)
        });
    let meta = |k: &str| {
        obj.parent_metadata
            .as_ref()
            .and_then(|m| maybe_string(m.get(k)))
    };
    let org_id = existing
        .as_ref()
        .and_then(|e| e.org_id.clone())
        .or_else(|| subscription.as_ref().and_then(|s| s.org_id.clone()))
        .or_else(|| meta("orgId"));
    let user_id = existing
        .as_ref()
        .and_then(|e| e.user_id.clone())
        .or_else(|| subscription.as_ref().and_then(|s| s.user_id.clone()))
        .or_else(|| meta("userId"));
    // A payment can arrive before its invoice; fill what it couldn't resolve.
    let invoice_key = Some(obj.id.clone());
    let payments: Vec<StripePayment> = ctx
        .db
        .stripe_payment()
        .iter()
        .filter(|p| p.stripe_invoice_id == invoice_key)
        .collect();
    for payment in payments {
        let mut filled = payment.clone();
        filled.stripe_customer_id = payment
            .stripe_customer_id
            .clone()
            .or(Some(customer_id.clone()));
        filled.org_id = payment.org_id.clone().or(org_id.clone());
        filled.user_id = payment.user_id.clone().or(user_id.clone());
        if filled.stripe_customer_id != payment.stripe_customer_id
            || filled.org_id != payment.org_id
            || filled.user_id != payment.user_id
        {
            ctx.db
                .stripe_payment()
                .stripe_payment_intent_id()
                .update(filled);
        }
    }
    let amount_due = big(obj.amount_due)?
        .or(existing.as_ref().map(|e| e.amount_due))
        .unwrap_or(0);
    let amount_paid = big(obj.amount_paid)?
        .or(existing.as_ref().map(|e| e.amount_paid))
        .unwrap_or(0);
    let created_unix = big(obj.created)?
        .or(existing.as_ref().map(|e| e.created_unix))
        .unwrap_or(0);
    upsert_invoice(
        ctx,
        ctx.timestamp,
        StripeInvoice {
            stripe_invoice_id: obj.id,
            stripe_customer_id: customer_id,
            stripe_subscription_id: subscription_id,
            status,
            amount_due,
            amount_paid,
            created_unix,
            org_id,
            user_id,
            event_created_unix,
            inserted_at: ctx.timestamp,
            updated_at: ctx.timestamp,
        },
    );
    Ok(S::Processed)
}

fn dispatch_event(
    ctx: &ReducerContext,
    event: Event,
    event_created_unix: i64,
) -> Result<S, String> {
    let now = ctx.timestamp;
    match event {
        Event::Customer {
            id,
            email,
            name,
            metadata,
        } => {
            let existing = ctx.db.stripe_customer().stripe_customer_id().find(&id);
            if existing.is_some_and(|e| event_created_unix < e.event_created_unix) {
                return Ok(S::Ignored);
            }
            let (metadata_json, _, user_id) = record_info(&metadata);
            upsert_customer(
                ctx,
                now,
                StripeCustomer {
                    stripe_customer_id: id,
                    app_user_id: None,
                    email,
                    name,
                    metadata_json,
                    user_id,
                    event_created_unix,
                    created_at: now,
                    updated_at: now,
                },
            );
            Ok(S::Processed)
        }
        Event::Subscription {
            deleted,
            id,
            customer,
            status,
            current_period_end,
            cancel_at,
            cancel_at_period_end,
            first_item,
            metadata,
        } => {
            let status = if deleted {
                "canceled".to_string()
            } else {
                status
            };
            let existing = ctx
                .db
                .stripe_subscription()
                .stripe_subscription_id()
                .find(&id);
            if is_stale(
                existing
                    .as_ref()
                    .map(|e| (e.event_created_unix, subscription_status_rank(&e.status))),
                (event_created_unix, subscription_status_rank(&status)),
            ) {
                return Ok(S::Ignored);
            }
            let current_period_end_unix =
                big(first_item.as_ref().and_then(|i| i.current_period_end))?
                    .or(big(current_period_end)?)
                    .unwrap_or(0);
            let cancel_at_unix = big(cancel_at)?;
            let cancel_at_period_end = cancel_at_period_end.unwrap_or_else(|| {
                derive_cancel_at_period_end(cancel_at_unix, current_period_end_unix)
            });
            let (metadata_json, org_id, user_id) = record_info(&metadata);
            upsert_subscription(
                ctx,
                now,
                StripeSubscription {
                    stripe_subscription_id: id,
                    stripe_customer_id: customer,
                    status,
                    current_period_end_unix,
                    cancel_at_period_end,
                    cancel_at_unix,
                    quantity: big(first_item.as_ref().and_then(|i| i.quantity))?,
                    price_id: first_item.and_then(|i| i.price_id),
                    metadata_json,
                    org_id,
                    user_id,
                    event_created_unix,
                    inserted_at: now,
                    updated_at: now,
                },
            );
            Ok(S::Processed)
        }
        Event::Checkout {
            failed,
            id,
            status,
            payment_status,
            mode,
            customer,
            metadata,
        } => {
            // Stripe reports a failed delayed payment as `unpaid`; record it as `failed`.
            let payment_status = if failed {
                "failed".to_string()
            } else {
                payment_status
            };
            let existing = ctx
                .db
                .stripe_checkout_session()
                .stripe_checkout_session_id()
                .find(&id);
            if is_stale(
                existing.as_ref().map(|e| {
                    (
                        e.event_created_unix,
                        checkout_payment_status_rank(&e.payment_status),
                    )
                }),
                (
                    event_created_unix,
                    checkout_payment_status_rank(&payment_status),
                ),
            ) {
                return Ok(S::Ignored);
            }
            let (metadata_json, _, _) = record_info(&metadata);
            upsert_checkout_session(
                ctx,
                now,
                StripeCheckoutSession {
                    stripe_checkout_session_id: id,
                    stripe_customer_id: customer.flatten(),
                    status: status.unwrap_or_else(|| "complete".into()),
                    payment_status,
                    mode: mode.unwrap_or_else(|| "payment".into()),
                    metadata_json,
                    event_created_unix,
                    inserted_at: now,
                    updated_at: now,
                },
            );
            Ok(S::Processed)
        }
        Event::Invoice {
            default_status,
            obj,
        } => {
            let status = obj
                .status
                .clone()
                .unwrap_or_else(|| default_status.to_string());
            sync_invoice_event(ctx, obj, status, event_created_unix)
        }
        Event::InvoicePayment {
            invoice,
            amount_paid,
            currency,
            created,
            payment_intent,
        } => {
            let Some(payment_intent_id) = payment_intent else {
                return Ok(S::Ignored);
            };
            let existing = ctx
                .db
                .stripe_payment()
                .stripe_payment_intent_id()
                .find(&payment_intent_id);
            let inv = ctx.db.stripe_invoice().stripe_invoice_id().find(&invoice);
            let amount = match &existing {
                Some(e) => e.amount,
                None => big(amount_paid)?.unwrap_or(0),
            };
            let created_unix = match &existing {
                Some(e) => e.created_unix,
                None => created.big()?,
            };
            upsert_payment(
                ctx,
                now,
                StripePayment {
                    stripe_payment_intent_id: payment_intent_id,
                    stripe_customer_id: inv.as_ref().map(|i| i.stripe_customer_id.clone()),
                    stripe_invoice_id: Some(invoice),
                    amount,
                    currency: existing.as_ref().map_or(currency, |e| e.currency.clone()),
                    status: existing
                        .as_ref()
                        .map_or_else(|| "succeeded".to_string(), |e| e.status.clone()),
                    created_unix,
                    metadata_json: None,
                    org_id: inv.as_ref().and_then(|i| i.org_id.clone()),
                    user_id: inv.as_ref().and_then(|i| i.user_id.clone()),
                    inserted_at: now,
                    updated_at: now,
                },
            );
            Ok(S::Processed)
        }
        Event::PaymentIntent {
            id,
            customer,
            invoice,
            amount,
            currency,
            status,
            created,
            metadata,
        } => {
            let (metadata_json, org_id, user_id) = record_info(&metadata);
            upsert_payment(
                ctx,
                now,
                StripePayment {
                    stripe_payment_intent_id: id,
                    stripe_customer_id: customer.flatten(),
                    stripe_invoice_id: invoice.flatten(),
                    amount: big(amount)?.unwrap_or(0),
                    currency: currency.unwrap_or_else(|| "unknown".into()),
                    status: status.unwrap_or_else(|| "succeeded".into()),
                    created_unix: big(created)?.unwrap_or(0),
                    metadata_json,
                    org_id,
                    user_id,
                    inserted_at: now,
                    updated_at: now,
                },
            );
            Ok(S::Processed)
        }
    }
}

/// `id`, `type` and `livemode` from a signed payload.
pub fn parse_stripe_event_metadata(payload_json: &str) -> Option<(String, String, bool)> {
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(payload_json) else {
        return None;
    };
    let id = o.get("id")?.as_str()?.to_string();
    let t = o.get("type")?.as_str()?.to_string();
    Some((
        id,
        t,
        o.get("livemode").and_then(Value::as_bool).unwrap_or(false),
    ))
}

/// Store and apply a verified event (shared by the reducer and the HTTP route). Returns
/// `(applied, status, error)`; `applied == false` means a processed/ignored duplicate.
fn store_and_apply(
    ctx: &ReducerContext,
    event_id: &str,
    event_type: &str,
    livemode: bool,
    payload_json: &str,
) -> Result<(bool, S, Option<String>), String> {
    let existing = ctx
        .db
        .stripe_webhook_event()
        .event_id()
        .find(event_id.to_string());
    if let Some(e) = &existing
        && matches!(e.status, S::Processed | S::Ignored)
    {
        return Ok((false, e.status, None));
    }
    if existing.is_none() {
        ctx.db.stripe_webhook_event().insert(StripeWebhookEvent {
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            livemode,
            payload_json: payload_json.to_string(),
            status: S::Received,
            error_message: None,
            received_at: ctx.timestamp,
            processed_at: None,
        });
    }
    let payload = existing
        .as_ref()
        .map_or(payload_json, |e| e.payload_json.as_str());
    let (status, error) = apply_stripe_event(ctx, payload)?;
    update_webhook_status(ctx, event_id, status, error.clone());
    Ok((true, status, error))
}

/// Relay ingest: verifies the signature and the separately supplied metadata, then stores and
/// applies the event. Rejections write nothing; a `Failed` row commits.
#[spacetimedb::reducer]
pub fn ingest_stripe_webhook(
    ctx: &ReducerContext,
    event_id: String,
    event_type: String,
    livemode: bool,
    payload_json: String,
    signature_header: Option<String>,
) -> Result<(), String> {
    if event_id.is_empty()
        || js_len(&event_id) > MAX_WEBHOOK_METADATA_LENGTH
        || event_type.is_empty()
        || js_len(&event_type) > MAX_WEBHOOK_METADATA_LENGTH
    {
        return Err(errors::WEBHOOK_METADATA_INVALID.into());
    }
    if js_len(&payload_json) > MAX_WEBHOOK_BODY_LENGTH {
        return Err(errors::WEBHOOK_PAYLOAD_TOO_LARGE.into());
    }
    if signature_header.as_deref().map_or(0, js_len) > MAX_WEBHOOK_HEADER_LENGTH {
        return Err(errors::WEBHOOK_SIGNATURE_TOO_LARGE.into());
    }
    let secret = ctx
        .db
        .stripe_config()
        .singleton()
        .find(true)
        .and_then(|c| c.webhook_signing_secret)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| errors::WEBHOOK_SECRET_NOT_CONFIGURED.to_string())?;
    crypto::verify_stripe_signature(&crypto::StripeVerifyOpts {
        raw_body: &payload_json,
        signature_header: signature_header.as_deref().unwrap_or(""),
        secret: &secret,
        tolerance_seconds: Some(crypto::DEFAULT_TOLERANCE_SECONDS),
        now_seconds: unix_seconds(ctx.timestamp),
    })
    .map_err(|r| format!("{}:{}", errors::WEBHOOK_SIGNATURE_MISMATCH, r.code()))?;
    let (signed_id, signed_type, signed_livemode) = parse_stripe_event_metadata(&payload_json)
        .ok_or_else(|| errors::WEBHOOK_PAYLOAD_MISSING_METADATA.to_string())?;
    if event_id != signed_id || event_type != signed_type || livemode != signed_livemode {
        return Err(errors::WEBHOOK_METADATA_MISMATCH.into());
    }
    store_and_apply(
        ctx,
        &signed_id,
        &signed_type,
        signed_livemode,
        &payload_json,
    )
    .map(|_| ())
}

/// Admins only: re-apply a stored event (the same staleness rules apply).
#[spacetimedb::reducer]
pub fn stripe_replay_webhook_event(ctx: &ReducerContext, event_id: String) -> Result<(), String> {
    if !is_admin(ctx, ctx.sender()) {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    let event = ctx
        .db
        .stripe_webhook_event()
        .event_id()
        .find(&event_id)
        .ok_or_else(|| errors::WEBHOOK_EVENT_NOT_FOUND.to_string())?;
    let (status, error) = apply_stripe_event(ctx, &event.payload_json)?;
    update_webhook_status(ctx, &event_id, status, error);
    Ok(())
}

/// Scheduled hourly by `install`: deletes up to 500 events received over 30 days ago.
#[spacetimedb::reducer]
pub fn stripe_prune_webhook_events(
    ctx: &ReducerContext,
    _tick: StripeWebhookPruneTick,
) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    prune_webhook_events(ctx, ctx.timestamp);
    Ok(())
}

/// The retention sweep, callable from your own code (e.g. tests or an admin reducer).
pub fn prune_webhook_events(ctx: &ReducerContext, now: Timestamp) -> usize {
    let cutoff = Timestamp::from_micros_since_unix_epoch(
        now.to_micros_since_unix_epoch() - WEBHOOK_EVENT_RETENTION_MICROS,
    );
    let expired: Vec<String> = ctx
        .db
        .stripe_webhook_event()
        .received_at()
        .filter(..cutoff)
        .take(WEBHOOK_PRUNE_BATCH)
        .map(|e| e.event_id)
        .collect();
    let n = expired.len();
    for id in expired {
        ctx.db.stripe_webhook_event().event_id().delete(&id);
    }
    n
}

fn json_response(status: u16, body: String) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from_bytes(body))
        .expect("valid response")
}

fn error_json(status: u16, error: &str) -> Response {
    let mut m = Map::new();
    m.insert("error".into(), Value::String(error.into()));
    json_response(status, Value::Object(m).to_string())
}

/// HTTP route for Stripe's webhooks: register it on your router, e.g.
/// `.post("/stripe/webhook", stripe::stripe_webhook)`.
#[handler]
pub fn stripe_webhook(ctx: &mut HandlerContext, req: Request) -> Response {
    let header = |name: &str| -> Option<String> {
        let values: Vec<String> = req
            .headers()
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();
        (!values.is_empty()).then(|| values.join(", "))
    };
    let signature = header("stripe-signature");
    if req.method().as_str() != "POST" {
        return error_json(405, "method not allowed");
    }
    let content_length = header("content-length")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    if content_length > MAX_WEBHOOK_BODY_LENGTH as f64 {
        return error_json(413, "payload too large");
    }
    if signature.as_deref().map_or(0, js_len) > MAX_WEBHOOK_HEADER_LENGTH {
        return error_json(431, "signature header too large");
    }
    let payload = req.into_body().into_string_lossy();
    if payload.is_empty() {
        return error_json(400, "empty body");
    }
    if payload.len() > MAX_WEBHOOK_BODY_LENGTH {
        return error_json(413, "payload too large");
    }
    let secret = ctx.with_tx(|tx| {
        tx.db
            .stripe_config()
            .singleton()
            .find(true)
            .and_then(|c| c.webhook_signing_secret)
            .filter(|s| !s.is_empty())
    });
    let Some(secret) = secret else {
        return error_json(503, "webhook signing secret not configured");
    };
    if let Err(reason) = crypto::verify_stripe_signature(&crypto::StripeVerifyOpts {
        raw_body: &payload,
        signature_header: signature.as_deref().unwrap_or(""),
        secret: &secret,
        tolerance_seconds: Some(crypto::DEFAULT_TOLERANCE_SECONDS),
        now_seconds: unix_seconds(ctx.timestamp),
    }) {
        return error_json(401, reason.code());
    }
    let Some((event_id, event_type, livemode)) = parse_stripe_event_metadata(&payload) else {
        return error_json(400, "missing or invalid event metadata");
    };
    if event_id.is_empty()
        || js_len(&event_id) > MAX_WEBHOOK_METADATA_LENGTH
        || event_type.is_empty()
        || js_len(&event_type) > MAX_WEBHOOK_METADATA_LENGTH
    {
        return error_json(400, "invalid event metadata");
    }
    match ctx.try_with_tx(|tx| store_and_apply(tx, &event_id, &event_type, livemode, &payload)) {
        Err(e) => {
            log::error!("stripe webhook processing failed for {event_id}: {e}");
            error_json(500, "webhook processing failed")
        }
        Ok((applied, status, error)) => {
            let ok = status != S::Failed;
            let mut m = Map::new();
            m.insert("ok".into(), Value::Bool(ok));
            m.insert("eventId".into(), Value::String(event_id));
            m.insert(
                "kind".into(),
                Value::String(if applied { "applied" } else { "duplicate" }.into()),
            );
            let mut tag = Map::new();
            tag.insert("tag".into(), Value::String(format!("{status:?}")));
            m.insert("status".into(), Value::Object(tag));
            if let Some(e) = error {
                m.insert("error".into(), Value::String(e));
            }
            json_response(if ok { 200 } else { 400 }, Value::Object(m).to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Billing
// ---------------------------------------------------------------------------

/// Create a Stripe customer and mirror it locally. Returns its id.
fn create_customer_in_stripe_and_sync(
    ctx: &mut ProcedureContext,
    cfg: &StripeConfig,
    email: Option<String>,
    name: Option<String>,
    metadata_json: Option<String>,
    idempotency_key: Option<String>,
) -> Result<String, String> {
    let mut pairs = vec![
        ("email".to_string(), email.clone()),
        ("name".to_string(), name.clone()),
    ];
    pairs.extend(metadata_json_to_form_pairs(
        "metadata",
        metadata_json.as_deref(),
    ));
    let key = idempotency_key
        .filter(|k| !k.is_empty())
        .map(|k| format!("create_customer_{k}"));
    let result = call_stripe(
        ctx,
        cfg,
        "POST",
        "/v1/customers",
        Some(&form_pairs_to_body(&pairs)),
        key.as_deref(),
    )?;
    ensure_ok(&result, errors::CREATE_CUSTOMER_FAILED)?;
    let customer_id = match serde_json::from_str::<Value>(&result.body) {
        Ok(Value::Object(o)) => match o.get("id") {
            Some(Value::String(id)) => id.clone(),
            Some(_) => {
                return Err(format!(
                    "{}:{}",
                    errors::CREATE_CUSTOMER_INVALID_RESPONSE,
                    issue_type("string", "id")
                ));
            }
            None => {
                return Err(format!(
                    "{}:{}",
                    errors::CREATE_CUSTOMER_INVALID_RESPONSE,
                    issue_missing("id")
                ));
            }
        },
        _ => {
            return Err(format!(
                "{}:{}",
                errors::CREATE_CUSTOMER_INVALID_RESPONSE,
                issue_type("Object", "")
            ));
        }
    };
    let (meta_json, _, user_id) = coerce_metadata_from_json(metadata_json.as_deref());
    let now = ctx.timestamp;
    ctx.with_tx(|tx| {
        upsert_customer(
            tx,
            now,
            StripeCustomer {
                stripe_customer_id: customer_id.clone(),
                app_user_id: None,
                email: email.clone(),
                name: name.clone(),
                metadata_json: meta_json.clone(),
                user_id: user_id.clone(),
                event_created_unix: unix_seconds(now),
                created_at: now,
                updated_at: now,
            },
        )
    });
    Ok(customer_id)
}

fn find_user_customer_id(ctx: &mut ProcedureContext, user_id: &str) -> Option<String> {
    let key = Some(user_id.to_string());
    ctx.with_tx(|tx| {
        if let Some(c) = tx.db.stripe_customer().iter().find(|c| c.user_id == key) {
            return Some(c.stripe_customer_id);
        }
        if let Some(s) = tx
            .db
            .stripe_subscription()
            .iter()
            .find(|s| s.user_id == key)
        {
            return Some(s.stripe_customer_id);
        }
        tx.db
            .stripe_payment()
            .iter()
            .filter(|p| p.user_id == key)
            .find_map(|p| p.stripe_customer_id.filter(|c| !c.is_empty()))
    })
}

/// The Stripe customer for an application user, created with `metadata.userId` when none
/// exists. Matches by `userId` only. No authorization: resolve `user_id` from trusted context.
pub fn get_or_create_user_customer(
    ctx: &mut ProcedureContext,
    user_id: &str,
    email: Option<String>,
    name: Option<String>,
) -> Result<StripeGetOrCreateCustomerResult, String> {
    if let Some(customer_id) = find_user_customer_id(ctx, user_id).filter(|c| !c.is_empty()) {
        return Ok(StripeGetOrCreateCustomerResult {
            customer_id,
            is_new: false,
        });
    }
    let cfg = config(ctx)?;
    let mut meta = Map::new();
    meta.insert("userId".into(), Value::String(user_id.to_string()));
    let customer_id = create_customer_in_stripe_and_sync(
        ctx,
        &cfg,
        email,
        name,
        Some(js_json(&Value::Object(meta))),
        Some(user_id.to_string()),
    )?;
    Ok(StripeGetOrCreateCustomerResult {
        customer_id,
        is_new: true,
    })
}

/// Arguments for [`create_checkout_session_in_stripe`] (the TS `CheckoutSessionArgs`).
#[derive(Clone, Debug, Default)]
pub struct CheckoutSessionArgs {
    pub items: Vec<StripeCheckoutLineItem>,
    pub customer_id: Option<String>,
    pub mode: String,
    pub success_url: String,
    pub cancel_url: String,
    pub metadata_json: Option<String>,
    pub subscription_metadata_json: Option<String>,
    pub payment_intent_metadata_json: Option<String>,
}

/// Create a Checkout session. All items must share the session's mode.
pub fn create_checkout_session_in_stripe(
    ctx: &mut ProcedureContext,
    args: CheckoutSessionArgs,
) -> Result<StripeCheckoutSessionResult, String> {
    let cfg = config(ctx)?;
    if args.items.is_empty() {
        return Err(errors::CHECKOUT_SESSION_REQUIRES_ITEMS.into());
    }
    let mut pairs: Vec<(String, Option<String>)> = vec![
        ("mode".into(), Some(args.mode.clone())),
        ("success_url".into(), Some(args.success_url.clone())),
        ("cancel_url".into(), Some(args.cancel_url.clone())),
        ("customer".into(), args.customer_id.clone()),
    ];
    for (i, item) in args.items.iter().enumerate() {
        pairs.push((
            format!("line_items[{i}][price]"),
            Some(item.price_id.clone()),
        ));
        pairs.push((
            format!("line_items[{i}][quantity]"),
            Some(item.quantity.to_string()),
        ));
    }
    pairs.extend(metadata_json_to_form_pairs(
        "metadata",
        args.metadata_json.as_deref(),
    ));
    if args.mode == "subscription" {
        pairs.extend(metadata_json_to_form_pairs(
            "subscription_data[metadata]",
            args.subscription_metadata_json.as_deref(),
        ));
    }
    if args.mode == "payment" {
        pairs.extend(metadata_json_to_form_pairs(
            "payment_intent_data[metadata]",
            args.payment_intent_metadata_json.as_deref(),
        ));
    }
    let response = call_stripe(
        ctx,
        &cfg,
        "POST",
        "/v1/checkout/sessions",
        Some(&form_pairs_to_body(&pairs)),
        None,
    )?;
    ensure_ok(&response, errors::CHECKOUT_SESSION_FAILED)?;
    let invalid = |issue: String| format!("{}:{issue}", errors::CHECKOUT_SESSION_INVALID_RESPONSE);
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(&response.body) else {
        return Err(invalid(issue_type("Object", "")));
    };
    let session_id = req_str(&o, "id", "").map_err(invalid)?;
    let url = opt_str_or_null(&o, "url", "").map_err(invalid)?;
    Ok(StripeCheckoutSessionResult { session_id, url })
}

/// Arguments for [`create_user_checkout_session`].
#[derive(Clone, Debug, Default)]
pub struct UserCheckoutArgs {
    pub user_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub items: Vec<StripeCheckoutLineItem>,
    /// `payment` or `subscription`.
    pub mode: String,
    pub success_url: String,
    pub cancel_url: String,
    pub metadata: Vec<(String, String)>,
}

/// A Checkout session for an application user's own customer. `userId` goes into the session,
/// subscription and PaymentIntent metadata. No authorization: resolve the user from trusted
/// context and supply server-owned prices and return URLs.
pub fn create_user_checkout_session(
    ctx: &mut ProcedureContext,
    args: UserCheckoutArgs,
) -> Result<StripeCheckoutSessionResult, String> {
    let customer =
        get_or_create_user_customer(ctx, &args.user_id, args.email.clone(), args.name.clone())?;
    let mut meta = Map::new();
    for (k, v) in &args.metadata {
        meta.insert(k.clone(), Value::String(v.clone()));
    }
    meta.insert("userId".into(), Value::String(args.user_id.clone()));
    let metadata_json = js_json(&Value::Object(meta));
    create_checkout_session_in_stripe(
        ctx,
        CheckoutSessionArgs {
            items: args.items,
            customer_id: Some(customer.customer_id),
            mode: args.mode,
            success_url: args.success_url,
            cancel_url: args.cancel_url,
            metadata_json: Some(metadata_json.clone()),
            subscription_metadata_json: Some(metadata_json.clone()),
            payment_intent_metadata_json: Some(metadata_json),
        },
    )
}

/// Admins only: confirms a price exists and is active.
#[spacetimedb::procedure]
pub fn validate_stripe_price(
    ctx: &mut ProcedureContext,
    price_id: String,
) -> Result<ValidateStripePriceResult, String> {
    require_procedure_admin(ctx)?;
    let cfg = config(ctx)?;
    let response = call_stripe(
        ctx,
        &cfg,
        "GET",
        &format!("/v1/prices/{price_id}"),
        None,
        None,
    )?;
    let parsed = serde_json::from_str::<Value>(&response.body).ok();
    let empty = Value::Object(Map::new());
    if !(200..300).contains(&response.status) {
        let err = parsed
            .as_ref()
            .filter(|p| is_record(p))
            .and_then(|p| p.get("error"))
            .filter(|e| is_record(e));
        return Ok(ValidateStripePriceResult {
            valid: false,
            status: response.status,
            active: None,
            currency: None,
            unit_amount: None,
            livemode: None,
            price_type: None,
            message: Some(
                maybe_string(err.and_then(|e| e.get("message")))
                    .unwrap_or_else(|| format!("Stripe returned {}.", response.status)),
            ),
            code: maybe_string(err.and_then(|e| e.get("code"))),
            error_type: maybe_string(err.and_then(|e| e.get("type"))),
        });
    }
    let data = parsed.as_ref().filter(|p| is_record(p)).unwrap_or(&empty);
    Ok(ValidateStripePriceResult {
        valid: true,
        status: response.status,
        active: maybe_bool(data.get("active")),
        currency: maybe_string(data.get("currency")),
        unit_amount: maybe_i64(data.get("unit_amount")),
        livemode: maybe_bool(data.get("livemode")),
        price_type: maybe_string(data.get("type")),
        message: None,
        code: None,
        error_type: None,
    })
}

/// Admins only: fetch a Checkout session's state from Stripe.
#[spacetimedb::procedure]
pub fn stripe_get_remote_checkout_session(
    ctx: &mut ProcedureContext,
    session_id: String,
) -> Result<StripeRemoteCheckoutSessionResult, String> {
    require_procedure_admin(ctx)?;
    let cfg = config(ctx)?;
    let response = call_stripe(
        ctx,
        &cfg,
        "GET",
        &format!("/v1/checkout/sessions/{session_id}"),
        None,
        None,
    )?;
    let parsed = serde_json::from_str::<Value>(&response.body).ok();
    let empty = Value::Object(Map::new());
    if !(200..300).contains(&response.status) {
        let err = parsed
            .as_ref()
            .filter(|p| is_record(p))
            .and_then(|p| p.get("error"))
            .filter(|e| is_record(e));
        return Ok(StripeRemoteCheckoutSessionResult {
            ok: false,
            status: response.status,
            session_id: None,
            payment_status: None,
            session_status: None,
            mode: None,
            amount_total: None,
            currency: None,
            customer_id: None,
            payment_intent_id: None,
            message: Some(
                maybe_string(err.and_then(|e| e.get("message")))
                    .unwrap_or_else(|| format!("Stripe returned {}.", response.status)),
            ),
            code: maybe_string(err.and_then(|e| e.get("code"))),
            error_type: maybe_string(err.and_then(|e| e.get("type"))),
        });
    }
    let data = parsed.as_ref().filter(|p| is_record(p)).unwrap_or(&empty);
    Ok(StripeRemoteCheckoutSessionResult {
        ok: true,
        status: response.status,
        session_id: Some(maybe_string(data.get("id")).unwrap_or(session_id)),
        payment_status: maybe_string(data.get("payment_status")),
        session_status: maybe_string(data.get("status")),
        mode: maybe_string(data.get("mode")),
        amount_total: maybe_i64(data.get("amount_total")),
        currency: maybe_string(data.get("currency")),
        customer_id: maybe_id(data.get("customer")),
        payment_intent_id: maybe_id(data.get("payment_intent")),
        message: None,
        code: None,
        error_type: None,
    })
}

/// Admins only: how many webhook events are stored.
#[spacetimedb::procedure]
pub fn stripe_get_webhook_event_count(ctx: &mut ProcedureContext) -> Result<i64, String> {
    require_procedure_admin(ctx)?;
    Ok(ctx.with_tx(|tx| tx.db.stripe_webhook_event().count() as i64))
}

/// Admins only: replace a subscription's local metadata (and `orgId` / `userId`).
#[spacetimedb::procedure]
pub fn stripe_update_subscription_metadata(
    ctx: &mut ProcedureContext,
    stripe_subscription_id: String,
    metadata_json: String,
    org_id: Option<String>,
    user_id: Option<String>,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    let (parsed_json, parsed_org, parsed_user) = coerce_metadata_from_json(Some(&metadata_json));
    let now = ctx.timestamp;
    ctx.try_with_tx(|tx| {
        let existing = tx
            .db
            .stripe_subscription()
            .stripe_subscription_id()
            .find(&stripe_subscription_id)
            .ok_or_else(|| errors::SUBSCRIPTION_NOT_FOUND.to_string())?;
        let mut row = existing.clone();
        row.metadata_json = Some(parsed_json.clone().unwrap_or_else(|| metadata_json.clone()));
        row.org_id = org_id
            .clone()
            .or(parsed_org.clone())
            .or(existing.org_id.clone());
        row.user_id = user_id
            .clone()
            .or(parsed_user.clone())
            .or(existing.user_id.clone());
        upsert_subscription(tx, now, row);
        Ok(())
    })
}

/// Admins only.
#[spacetimedb::procedure]
pub fn stripe_get_or_create_customer(
    ctx: &mut ProcedureContext,
    user_id: String,
    email: Option<String>,
    name: Option<String>,
) -> Result<StripeGetOrCreateCustomerResult, String> {
    require_procedure_admin(ctx)?;
    get_or_create_user_customer(ctx, &user_id, email, name)
}

/// Admins only.
#[spacetimedb::procedure]
#[allow(clippy::too_many_arguments)]
pub fn stripe_create_checkout_session(
    ctx: &mut ProcedureContext,
    items: Vec<StripeCheckoutLineItem>,
    customer_id: Option<String>,
    mode: String,
    success_url: String,
    cancel_url: String,
    metadata_json: Option<String>,
    subscription_metadata_json: Option<String>,
    payment_intent_metadata_json: Option<String>,
) -> Result<StripeCheckoutSessionResult, String> {
    require_procedure_admin(ctx)?;
    create_checkout_session_in_stripe(
        ctx,
        CheckoutSessionArgs {
            items,
            customer_id,
            mode,
            success_url,
            cancel_url,
            metadata_json,
            subscription_metadata_json,
            payment_intent_metadata_json,
        },
    )
}

/// Admins only.
#[spacetimedb::procedure]
pub fn stripe_create_customer_portal_session(
    ctx: &mut ProcedureContext,
    customer_id: String,
    return_url: String,
) -> Result<StripePortalSessionResult, String> {
    require_procedure_admin(ctx)?;
    let cfg = config(ctx)?;
    let body = form_pairs_to_body(&[
        ("customer".into(), Some(customer_id)),
        ("return_url".into(), Some(return_url)),
    ]);
    let response = call_stripe(
        ctx,
        &cfg,
        "POST",
        "/v1/billing_portal/sessions",
        Some(&body),
        None,
    )?;
    ensure_ok(&response, errors::PORTAL_SESSION_FAILED)?;
    let invalid = |issue: String| format!("{}:{issue}", errors::PORTAL_SESSION_INVALID_RESPONSE);
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(&response.body) else {
        return Err(invalid(issue_type("Object", "")));
    };
    Ok(StripePortalSessionResult {
        url: req_str(&o, "url", "").map_err(invalid)?,
    })
}

fn patch_subscription(
    ctx: &mut ProcedureContext,
    cfg: &StripeConfig,
    subscription_id: &str,
    form_body: &str,
) -> Result<Value, String> {
    let response = call_stripe(
        ctx,
        cfg,
        "POST",
        &format!("/v1/subscriptions/{subscription_id}"),
        Some(form_body),
        None,
    )?;
    ensure_ok(&response, errors::SUBSCRIPTION_UPDATE_FAILED)?;
    serde_json::from_str::<Value>(&response.body)
        .ok()
        .filter(is_record)
        .ok_or_else(|| errors::SUBSCRIPTION_UPDATE_INVALID_RESPONSE.to_string())
}

fn sync_subscription_from_stripe(ctx: &mut ProcedureContext, sub: &Value) -> Result<(), String> {
    let subscription_id = maybe_string(sub.get("id")).filter(|s| !s.is_empty());
    let customer_id = maybe_id(sub.get("customer")).filter(|s| !s.is_empty());
    let status = maybe_string(sub.get("status")).filter(|s| !s.is_empty());
    let (Some(subscription_id), Some(customer_id), Some(status)) =
        (subscription_id, customer_id, status)
    else {
        return Err(errors::SUBSCRIPTION_PAYLOAD_MISSING_FIELDS.into());
    };
    let items = sub.get("items").filter(|v| is_record(v));
    let first = items
        .and_then(|i| i.get("data"))
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .filter(|v| is_record(v));
    let price = first.and_then(|f| f.get("price")).filter(|v| is_record(v));
    let current_period_end = maybe_i64(first.and_then(|f| f.get("current_period_end")))
        .or_else(|| maybe_i64(sub.get("current_period_end")))
        .unwrap_or(0);
    let cancel_at_unix = maybe_i64(sub.get("cancel_at"));
    let cancel_at_period_end = maybe_bool(sub.get("cancel_at_period_end"))
        .unwrap_or_else(|| derive_cancel_at_period_end(cancel_at_unix, current_period_end));
    let quantity = maybe_i64(first.and_then(|f| f.get("quantity")));
    let (metadata_json, org_id, user_id) = metadata_info(sub.get("metadata"));
    let price_id = maybe_string(price.and_then(|p| p.get("id")));
    let now = ctx.timestamp;
    ctx.with_tx(|tx| {
        upsert_subscription(
            tx,
            now,
            StripeSubscription {
                stripe_subscription_id: subscription_id.clone(),
                stripe_customer_id: customer_id.clone(),
                status: status.clone(),
                current_period_end_unix: current_period_end,
                cancel_at_period_end,
                cancel_at_unix,
                quantity,
                price_id: price_id.clone(),
                metadata_json: metadata_json.clone(),
                org_id: org_id.clone(),
                user_id: user_id.clone(),
                event_created_unix: unix_seconds(now),
                inserted_at: now,
                updated_at: now,
            },
        )
    });
    Ok(())
}

/// Admins only. Cancels at period end unless `cancel_at_period_end` is `Some(false)`.
#[spacetimedb::procedure]
pub fn stripe_cancel_subscription(
    ctx: &mut ProcedureContext,
    stripe_subscription_id: String,
    cancel_at_period_end: Option<bool>,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    let cfg = config(ctx)?;
    let sub = if cancel_at_period_end.unwrap_or(true) {
        patch_subscription(
            ctx,
            &cfg,
            &stripe_subscription_id,
            "cancel_at_period_end=true",
        )?
    } else {
        let response = call_stripe(
            ctx,
            &cfg,
            "DELETE",
            &format!("/v1/subscriptions/{stripe_subscription_id}"),
            None,
            None,
        )?;
        ensure_ok(&response, errors::SUBSCRIPTION_CANCEL_FAILED)?;
        serde_json::from_str::<Value>(&response.body)
            .ok()
            .filter(is_record)
            .ok_or_else(|| errors::SUBSCRIPTION_CANCEL_INVALID_RESPONSE.to_string())?
    };
    sync_subscription_from_stripe(ctx, &sub)
}

/// Admins only.
#[spacetimedb::procedure]
pub fn stripe_reactivate_subscription(
    ctx: &mut ProcedureContext,
    stripe_subscription_id: String,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    let cfg = config(ctx)?;
    let sub = patch_subscription(
        ctx,
        &cfg,
        &stripe_subscription_id,
        "cancel_at_period_end=false",
    )?;
    sync_subscription_from_stripe(ctx, &sub)
}

/// Admins only: sets the first line item's quantity.
#[spacetimedb::procedure]
pub fn stripe_update_subscription_quantity(
    ctx: &mut ProcedureContext,
    stripe_subscription_id: String,
    quantity: i64,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    let cfg = config(ctx)?;
    let get = call_stripe(
        ctx,
        &cfg,
        "GET",
        &format!("/v1/subscriptions/{stripe_subscription_id}"),
        None,
        None,
    )?;
    ensure_ok(&get, errors::SUBSCRIPTION_LOOKUP_FAILED)?;
    let existing = serde_json::from_str::<Value>(&get.body)
        .ok()
        .filter(is_record)
        .ok_or_else(|| errors::SUBSCRIPTION_LOOKUP_INVALID_RESPONSE.to_string())?;
    let item_id = existing
        .get("items")
        .filter(|v| is_record(v))
        .and_then(|i| i.get("data"))
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .filter(|v| is_record(v))
        .and_then(|f| maybe_string(f.get("id")))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| errors::SUBSCRIPTION_MISSING_LINE_ITEMS.to_string())?;
    let update = call_stripe(
        ctx,
        &cfg,
        "POST",
        &format!("/v1/subscription_items/{item_id}"),
        Some(&format!("quantity={quantity}")),
        None,
    )?;
    ensure_ok(&update, errors::SUBSCRIPTION_ITEM_UPDATE_FAILED)?;
    let now = ctx.timestamp;
    ctx.with_tx(|tx| {
        if let Some(mut local) = tx
            .db
            .stripe_subscription()
            .stripe_subscription_id()
            .find(&stripe_subscription_id)
        {
            local.quantity = Some(quantity);
            upsert_subscription(tx, now, local);
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Admin queries (at most 1,000 rows each)
// ---------------------------------------------------------------------------

fn admin_read<T>(
    ctx: &mut ProcedureContext,
    read: impl Fn(&ReducerContext) -> T,
) -> Result<T, String> {
    require_procedure_admin(ctx)?;
    Ok(ctx.with_tx(|tx| read(tx)))
}

fn latest_subscription(mut rows: Vec<StripeSubscription>) -> Option<StripeSubscription> {
    rows.sort_by(|a, b| {
        (
            a.inserted_at.to_micros_since_unix_epoch(),
            &a.stripe_subscription_id,
        )
            .cmp(&(
                b.inserted_at.to_micros_since_unix_epoch(),
                &b.stripe_subscription_id,
            ))
    });
    rows.pop()
}

fn subscriptions_where(
    tx: &ReducerContext,
    f: impl Fn(&StripeSubscription) -> bool,
) -> Vec<StripeSubscription> {
    let mut rows: Vec<StripeSubscription> = tx
        .db
        .stripe_subscription()
        .iter()
        .filter(|s| f(s))
        .collect();
    rows.sort_by_key(|s| s.inserted_at.to_micros_since_unix_epoch());
    rows
}

#[spacetimedb::procedure]
pub fn stripe_get_customer(
    ctx: &mut ProcedureContext,
    stripe_customer_id: String,
) -> Result<Option<StripeCustomer>, String> {
    admin_read(ctx, |tx| {
        tx.db
            .stripe_customer()
            .stripe_customer_id()
            .find(&stripe_customer_id)
    })
}

#[spacetimedb::procedure]
pub fn stripe_get_customer_by_user_id(
    ctx: &mut ProcedureContext,
    user_id: String,
) -> Result<Option<StripeCustomer>, String> {
    let key = Some(user_id);
    admin_read(ctx, |tx| {
        tx.db.stripe_customer().iter().find(|c| c.user_id == key)
    })
}

#[spacetimedb::procedure]
pub fn stripe_get_subscription(
    ctx: &mut ProcedureContext,
    stripe_subscription_id: String,
) -> Result<Option<StripeSubscription>, String> {
    admin_read(ctx, |tx| {
        tx.db
            .stripe_subscription()
            .stripe_subscription_id()
            .find(&stripe_subscription_id)
    })
}

#[spacetimedb::procedure]
pub fn stripe_list_subscriptions(
    ctx: &mut ProcedureContext,
    stripe_customer_id: String,
) -> Result<Vec<StripeSubscription>, String> {
    admin_read(ctx, |tx| {
        tx.db
            .stripe_subscription()
            .stripe_customer_id()
            .filter(&stripe_customer_id)
            .take(MAX_QUERY_ROWS)
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn stripe_get_subscription_by_org_id(
    ctx: &mut ProcedureContext,
    org_id: String,
) -> Result<Option<StripeSubscription>, String> {
    let key = Some(org_id);
    admin_read(ctx, |tx| {
        latest_subscription(subscriptions_where(tx, |s| s.org_id == key))
    })
}

#[spacetimedb::procedure]
pub fn stripe_list_subscriptions_by_org_id(
    ctx: &mut ProcedureContext,
    org_id: String,
) -> Result<Vec<StripeSubscription>, String> {
    let key = Some(org_id);
    admin_read(ctx, |tx| {
        subscriptions_where(tx, |s| s.org_id == key)
            .into_iter()
            .take(MAX_QUERY_ROWS)
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn stripe_list_subscriptions_by_user_id(
    ctx: &mut ProcedureContext,
    user_id: String,
) -> Result<Vec<StripeSubscription>, String> {
    let key = Some(user_id);
    admin_read(ctx, |tx| {
        subscriptions_where(tx, |s| s.user_id == key)
            .into_iter()
            .take(MAX_QUERY_ROWS)
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn stripe_get_payment(
    ctx: &mut ProcedureContext,
    stripe_payment_intent_id: String,
) -> Result<Option<StripePayment>, String> {
    admin_read(ctx, |tx| {
        tx.db
            .stripe_payment()
            .stripe_payment_intent_id()
            .find(&stripe_payment_intent_id)
    })
}

#[spacetimedb::procedure]
pub fn stripe_list_payments(
    ctx: &mut ProcedureContext,
    stripe_customer_id: String,
) -> Result<Vec<StripePayment>, String> {
    let key = Some(stripe_customer_id);
    admin_read(ctx, |tx| {
        tx.db
            .stripe_payment()
            .iter()
            .filter(|p| p.stripe_customer_id == key)
            .take(MAX_QUERY_ROWS)
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn stripe_list_payments_by_user_id(
    ctx: &mut ProcedureContext,
    user_id: String,
) -> Result<Vec<StripePayment>, String> {
    let key = Some(user_id);
    admin_read(ctx, |tx| {
        tx.db
            .stripe_payment()
            .iter()
            .filter(|p| p.user_id == key)
            .take(MAX_QUERY_ROWS)
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn stripe_list_payments_by_org_id(
    ctx: &mut ProcedureContext,
    org_id: String,
) -> Result<Vec<StripePayment>, String> {
    let key = Some(org_id);
    admin_read(ctx, |tx| {
        tx.db
            .stripe_payment()
            .iter()
            .filter(|p| p.org_id == key)
            .take(MAX_QUERY_ROWS)
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn stripe_list_invoices(
    ctx: &mut ProcedureContext,
    stripe_customer_id: String,
) -> Result<Vec<StripeInvoice>, String> {
    admin_read(ctx, |tx| {
        tx.db
            .stripe_invoice()
            .stripe_customer_id()
            .filter(&stripe_customer_id)
            .take(MAX_QUERY_ROWS)
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn stripe_list_invoices_by_org_id(
    ctx: &mut ProcedureContext,
    org_id: String,
) -> Result<Vec<StripeInvoice>, String> {
    let key = Some(org_id);
    admin_read(ctx, |tx| {
        tx.db
            .stripe_invoice()
            .iter()
            .filter(|i| i.org_id == key)
            .take(MAX_QUERY_ROWS)
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn stripe_list_invoices_by_user_id(
    ctx: &mut ProcedureContext,
    user_id: String,
) -> Result<Vec<StripeInvoice>, String> {
    let key = Some(user_id);
    admin_read(ctx, |tx| {
        tx.db
            .stripe_invoice()
            .iter()
            .filter(|i| i.user_id == key)
            .take(MAX_QUERY_ROWS)
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn stripe_get_checkout_session(
    ctx: &mut ProcedureContext,
    stripe_checkout_session_id: String,
) -> Result<Option<StripeCheckoutSession>, String> {
    admin_read(ctx, |tx| {
        tx.db
            .stripe_checkout_session()
            .stripe_checkout_session_id()
            .find(&stripe_checkout_session_id)
    })
}

#[spacetimedb::procedure]
pub fn stripe_list_checkout_sessions(
    ctx: &mut ProcedureContext,
    stripe_customer_id: String,
) -> Result<Vec<StripeCheckoutSession>, String> {
    let key = Some(stripe_customer_id);
    admin_read(ctx, |tx| {
        tx.db
            .stripe_checkout_session()
            .iter()
            .filter(|s| s.stripe_customer_id == key)
            .take(MAX_QUERY_ROWS)
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn stripe_get_webhook_event(
    ctx: &mut ProcedureContext,
    event_id: String,
) -> Result<Option<StripeWebhookEvent>, String> {
    admin_read(ctx, |tx| {
        tx.db.stripe_webhook_event().event_id().find(&event_id)
    })
}
