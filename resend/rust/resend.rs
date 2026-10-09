//! # resend.rs: send email through Resend and track delivery, for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/resend` (SpacetimeDB PR #5903, commit
//! `ae25f1da24b1723e28acd0038e75e598f33952c3`, package `spacetime-resend-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Same private tables (`resend_email`, `resend_delivery_event`, `resend_webhook_event`,
//! `resend_config`, `resend_admin_identity`), the same validation limits, error codes, status
//! ordering and webhook handling as the TS package. Needs `crypto.rs` from this repo (Svix
//! signatures) and `serde_json` with the `preserve_order` feature.
//!
//! ```ignore
//! #[path = "crypto.rs"] pub mod crypto;
//! #[path = "resend.rs"] pub mod resend;
//!
//! #[spacetimedb::reducer(init)]
//! pub fn init(ctx: &ReducerContext) { resend::install(ctx); }
//!
//! // Your own send procedure decides who may send to whom, then:
//! resend::send_email_request(ctx, resend::SendEmailArgs { to: vec![to], subject, html: Some(html), ..Default::default() })?;
//!
//! // Delivery webhooks: register the handler on your router.
//! #[spacetimedb::http::router]
//! fn router() -> Router { Router::new().post("/webhook/resend", resend::resend_webhook) }
//! ```
//!
//! Changes from the TypeScript original:
//! - Names: a drop-in shares your module's namespace, so the generic procedures and reducers get
//!   a `resend_` prefix (`send_email` → `resend_send_email`, `add_admin_identity` →
//!   `resend_add_admin_identity`, …) and the types a `Resend` prefix (`EmailStatus` →
//!   `ResendEmailStatus`, `WebhookEventStatus` → `ResendWebhookEventStatus`, `SendEmailResult`
//!   → `ResendSendEmailResult`). Names that already carry the package (`set_resend_config`,
//!   `ingest_resend_webhook`, `resend_api_request`, …) are unchanged.
//! - Procedures return `Result<T, String>` instead of throwing.
//! - `resend_list_emails_by_user_id` / `_by_org_id` scan the table: Rust can't filter an index on
//!   an `Option` column (the indexes exist, for schema parity). Fine for admin queries; for
//!   large tables keep your own index (e.g. a host table keyed by user).
//! - JSON the module writes (send bodies, stored tags) is serialized like JS `JSON.stringify`,
//!   including its key order (integer-like keys first).
//! - Payload validation is hand-written (no valibot), so `Failed` rows carry differently worded
//!   messages for the same inputs. Event timestamps accept ISO 8601 / RFC 3339 forms; JS
//!   `Date.parse` also accepts a few legacy formats.
//! - The API base URL can be overridden at build time with the `RESEND_API_BASE` environment
//!   variable (tests point it at a mock server). Default `https://api.resend.com`.

use crate::crypto;
use serde_json::{Map, Value, json};
use spacetimedb::http::{Body, HandlerContext, Request, Response, handler};
use spacetimedb::{Identity, ProcedureContext, ReducerContext, SpacetimeType, Table, Timestamp};

/// Where API calls go. Override at build time with `RESEND_API_BASE`.
pub const RESEND_API_BASE: &str = match option_env!("RESEND_API_BASE") {
    Some(base) => base,
    None => "https://api.resend.com",
};
const MAX_PATH_LENGTH: usize = 2048;
const MAX_JSON_BODY_LENGTH: usize = 256 * 1024;
const MAX_IDEMPOTENCY_KEY_LENGTH: usize = 256;
const MAX_QUERY_ROWS: usize = 1000;
const MAX_WEBHOOK_BODY_LENGTH: usize = 1024 * 1024;
const MAX_WEBHOOK_HEADER_LENGTH: usize = 8192;
const MAX_WEBHOOK_METADATA_LENGTH: usize = 255;

/// The `resend.*` codes, as in the TS `errors` object.
pub mod errors {
    pub const NOT_AUTHORIZED: &str = "resend.not_authorized";
    pub const CANNOT_REMOVE_LAST_ADMIN: &str = "resend.cannot_remove_last_admin";
    pub const CONFIG_NOT_SET: &str = "resend.config_not_set";
    pub const REQUEST_INVALID: &str = "resend.request_invalid";
    pub const REQUEST_PATH_INVALID: &str = "resend.request_path_invalid";
    pub const REQUEST_METHOD_INVALID: &str = "resend.request_method_invalid";
    pub const REQUEST_BODY_TOO_LARGE: &str = "resend.request_body_too_large";
    pub const IDEMPOTENCY_KEY_TOO_LONG: &str = "resend.idempotency_key_too_long";
    pub const SEND_EMAIL_INVALID_INPUT: &str = "resend.send_email_invalid_input";
    pub const SEND_EMAIL_NO_RECIPIENTS: &str = "resend.send_email_no_recipients";
    pub const SEND_EMAIL_TOO_MANY_RECIPIENTS: &str = "resend.send_email_too_many_recipients";
    pub const SEND_EMAIL_INVALID_FROM: &str = "resend.send_email_invalid_from";
    pub const SEND_EMAIL_INVALID_SUBJECT: &str = "resend.send_email_invalid_subject";
    pub const SEND_EMAIL_MISSING_CONTENT: &str = "resend.send_email_missing_content";
    pub const SEND_EMAIL_HTML_TOO_LARGE: &str = "resend.send_email_html_too_large";
    pub const SEND_EMAIL_TEXT_TOO_LARGE: &str = "resend.send_email_text_too_large";
    pub const SEND_EMAIL_INVALID_SCHEDULE: &str = "resend.send_email_invalid_schedule";
    pub const SEND_EMAIL_MISSING_FROM: &str = "resend.send_email_missing_from";
    pub const SEND_EMAIL_FAILED: &str = "resend.send_email_failed";
    pub const SEND_EMAIL_INVALID_RESPONSE: &str = "resend.send_email_invalid_response";
    pub const TO_TOO_MANY: &str = "resend.to_too_many";
    pub const TO_INVALID_ADDRESS: &str = "resend.to_invalid_address";
    pub const CC_TOO_MANY: &str = "resend.cc_too_many";
    pub const CC_INVALID_ADDRESS: &str = "resend.cc_invalid_address";
    pub const BCC_TOO_MANY: &str = "resend.bcc_too_many";
    pub const BCC_INVALID_ADDRESS: &str = "resend.bcc_invalid_address";
    pub const REPLY_TO_TOO_MANY: &str = "resend.reply_to_too_many";
    pub const REPLY_TO_INVALID_ADDRESS: &str = "resend.reply_to_invalid_address";
    pub const TAGS_TOO_LARGE: &str = "resend.tags_too_large";
    pub const TAGS_INVALID_JSON: &str = "resend.tags_invalid_json";
    pub const HEADERS_TOO_LARGE: &str = "resend.headers_too_large";
    pub const HEADERS_INVALID_JSON: &str = "resend.headers_invalid_json";
    pub const CANCEL_EMAIL_FAILED: &str = "resend.cancel_email_failed";
    pub const WEBHOOK_METHOD_NOT_ALLOWED: &str = "resend.webhook_method_not_allowed";
    pub const WEBHOOK_METADATA_INVALID: &str = "resend.webhook_metadata_invalid";
    pub const WEBHOOK_METADATA_MISMATCH: &str = "resend.webhook_metadata_mismatch";
    pub const WEBHOOK_PAYLOAD_TOO_LARGE: &str = "resend.webhook_payload_too_large";
    pub const WEBHOOK_HEADER_TOO_LARGE: &str = "resend.webhook_header_too_large";
    pub const WEBHOOK_SECRET_NOT_CONFIGURED: &str = "resend.webhook_secret_not_configured";
    pub const WEBHOOK_SIGNATURE_MISMATCH: &str = "resend.webhook_signature_mismatch";
    pub const WEBHOOK_PAYLOAD_INVALID: &str = "resend.webhook_payload_invalid";
    pub const WEBHOOK_EVENT_NOT_FOUND: &str = "resend.webhook_event_not_found";
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Webhook delivery state. Received = accepted, Processed = applied, Ignored = an event type
/// this module doesn't handle, Failed = invalid payload.
#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResendWebhookEventStatus {
    Received,
    Processed,
    Ignored,
    Failed,
}

/// Lifecycle of a tracked email; tags match Resend's `email.<type>` events.
#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResendEmailStatus {
    Queued,
    Sent,
    Delivered,
    DeliveryDelayed,
    Bounced,
    Failed,
    Cancelled,
}

impl ResendEmailStatus {
    /// Later stages win; earlier ones can't undo them.
    fn order(self) -> u8 {
        match self {
            Self::Queued => 0,
            Self::Sent => 1,
            Self::DeliveryDelayed => 2,
            Self::Delivered => 3,
            Self::Cancelled => 4,
            Self::Failed => 5,
            Self::Bounced => 6,
        }
    }
}

#[spacetimedb::table(accessor = resend_email)]
#[derive(Clone, Debug)]
pub struct ResendEmail {
    #[primary_key]
    pub resend_id: String,
    pub from_address: String,
    pub to_addresses_json: String,
    pub subject: Option<String>,
    #[index(btree)]
    pub status: ResendEmailStatus,
    pub last_error: Option<String>,
    pub bounced_at: Option<Timestamp>,
    pub bounce_json: Option<String>,
    pub failed_at: Option<Timestamp>,
    pub failure_reason: Option<String>,
    pub complained: bool,
    pub complained_at: Option<Timestamp>,
    pub opened: bool,
    pub opened_at: Option<Timestamp>,
    pub clicked: bool,
    pub clicked_at: Option<Timestamp>,
    pub delivered_at: Option<Timestamp>,
    pub sent_at: Option<Timestamp>,
    pub html: Option<String>,
    pub text: Option<String>,
    pub tags_json: Option<String>,
    #[index(btree)]
    pub user_id: Option<String>,
    #[index(btree)]
    pub org_id: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub status_updated_at: Option<Timestamp>,
}

#[spacetimedb::table(accessor = resend_delivery_event)]
#[derive(Clone, Debug)]
pub struct ResendDeliveryEvent {
    #[primary_key]
    pub event_id: String,
    #[index(btree)]
    pub resend_id: String,
    pub event_type: String,
    pub created_at_iso: String,
    pub detail_json: Option<String>,
    pub inserted_at: Timestamp,
}

/// Private: raw payloads and signature headers are operational data.
#[spacetimedb::table(accessor = resend_webhook_event)]
#[derive(Clone, Debug)]
pub struct ResendWebhookEvent {
    #[primary_key]
    pub event_id: String,
    pub event_type: String,
    pub payload_json: String,
    pub signature_header: Option<String>,
    pub timestamp_header: Option<String>,
    #[index(btree)]
    pub status: ResendWebhookEventStatus,
    pub error_message: Option<String>,
    pub received_at: Timestamp,
    pub processed_at: Option<Timestamp>,
}

/// Private singleton; secrets never leak through subscriptions.
#[spacetimedb::table(accessor = resend_config)]
#[derive(Clone)]
pub struct ResendConfig {
    #[primary_key]
    pub singleton: bool,
    pub api_key: String,
    pub webhook_signing_secret: Option<String>,
    pub default_from: Option<String>,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = resend_admin_identity)]
pub struct ResendAdminIdentity {
    #[primary_key]
    pub identity: Identity,
    pub added_at: Timestamp,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ResendSendEmailResult {
    pub resend_id: String,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ResendConfigStatus {
    pub is_configured: bool,
    pub has_webhook_secret: bool,
    pub default_from: Option<String>,
    pub api_key_length: u16,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ResendApiRequestResult {
    pub status: u16,
    pub body: String,
}

/// Arguments for [`send_email_request`] (the TS `SendEmailArgs`).
#[derive(Clone, Debug, Default)]
pub struct SendEmailArgs {
    pub from: Option<String>,
    pub to: Vec<String>,
    pub subject: String,
    pub html: Option<String>,
    pub text: Option<String>,
    pub cc: Option<Vec<String>>,
    pub bcc: Option<Vec<String>>,
    pub reply_to: Option<Vec<String>>,
    pub tags_json: Option<String>,
    pub headers_json: Option<String>,
    pub scheduled_at: Option<String>,
    pub idempotency_key: Option<String>,
}

// ---------------------------------------------------------------------------
// Admins and config
// ---------------------------------------------------------------------------

/// Call from your `init` reducer: makes the publishing identity the first admin.
pub fn install(ctx: &ReducerContext) {
    if ctx
        .db
        .resend_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        ctx.db.resend_admin_identity().insert(ResendAdminIdentity {
            identity: ctx.sender(),
            added_at: ctx.timestamp,
        });
    }
}

pub fn is_admin(ctx: &ReducerContext, who: Identity) -> bool {
    ctx.db
        .resend_admin_identity()
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
pub fn resend_add_admin_identity(
    ctx: &mut ProcedureContext,
    identity: Identity,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    let now = ctx.timestamp;
    ctx.with_tx(|tx| {
        if tx
            .db
            .resend_admin_identity()
            .identity()
            .find(identity)
            .is_none()
        {
            tx.db.resend_admin_identity().insert(ResendAdminIdentity {
                identity,
                added_at: now,
            });
        }
    });
    Ok(())
}

#[spacetimedb::procedure]
pub fn resend_remove_admin_identity(
    ctx: &mut ProcedureContext,
    identity: Identity,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    ctx.try_with_tx(|tx| {
        if tx
            .db
            .resend_admin_identity()
            .identity()
            .find(identity)
            .is_none()
        {
            return Ok(());
        }
        if tx.db.resend_admin_identity().count() <= 1 {
            return Err(errors::CANNOT_REMOVE_LAST_ADMIN.to_string());
        }
        tx.db.resend_admin_identity().identity().delete(identity);
        Ok(())
    })
}

/// The stored config, or `resend.config_not_set`.
pub fn load_config(ctx: &ReducerContext) -> Result<ResendConfig, String> {
    ctx.db
        .resend_config()
        .singleton()
        .find(true)
        .ok_or_else(|| {
            format!(
                "{}: call set_resend_config(...) first",
                errors::CONFIG_NOT_SET
            )
        })
}

/// Admins only. `None` keeps the stored webhook secret / default sender.
#[spacetimedb::procedure]
pub fn set_resend_config(
    ctx: &mut ProcedureContext,
    api_key: String,
    webhook_signing_secret: Option<String>,
    default_from: Option<String>,
) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    let now = ctx.timestamp;
    ctx.with_tx(|tx| {
        let existing = tx.db.resend_config().singleton().find(true);
        let row = ResendConfig {
            singleton: true,
            api_key: api_key.clone(),
            webhook_signing_secret: webhook_signing_secret.clone().or_else(|| {
                existing
                    .as_ref()
                    .and_then(|e| e.webhook_signing_secret.clone())
            }),
            default_from: default_from
                .clone()
                .or_else(|| existing.as_ref().and_then(|e| e.default_from.clone())),
            updated_at: now,
        };
        if existing.is_some() {
            tx.db.resend_config().singleton().update(row);
        } else {
            tx.db.resend_config().insert(row);
        }
    });
    Ok(())
}

#[spacetimedb::procedure]
pub fn get_resend_config_status(ctx: &mut ProcedureContext) -> Result<ResendConfigStatus, String> {
    require_procedure_admin(ctx)?;
    Ok(
        ctx.with_tx(|tx| match tx.db.resend_config().singleton().find(true) {
            None => ResendConfigStatus {
                is_configured: false,
                has_webhook_secret: false,
                default_from: None,
                api_key_length: 0,
            },
            Some(row) => ResendConfigStatus {
                is_configured: true,
                has_webhook_secret: row.webhook_signing_secret.is_some(),
                default_from: row.default_from,
                api_key_length: js_len(&row.api_key).min(u16::MAX as usize) as u16,
            },
        }),
    )
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Length in UTF-16 code units, like JS `string.length`.
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The first `n` UTF-16 code units, like JS `slice(0, n)` (never splitting a character).
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

/// JS `s.replace(/\s+/g, ' ')`.
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
fn js_json(v: &Value) -> String {
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
            let parts: Vec<String> = index
                .into_iter()
                .map(|(_, k, v)| (k, v))
                .chain(rest)
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

fn validate_address_list(
    values: Option<&Vec<String>>,
    too_many: &str,
    invalid: &str,
) -> Result<(), String> {
    let Some(values) = values else { return Ok(()) };
    if values.len() > 100 {
        return Err(too_many.into());
    }
    for v in values {
        if v.is_empty() || js_len(v) > 320 || has_control_character(v) {
            return Err(invalid.into());
        }
    }
    Ok(())
}

fn validate_json(
    value: Option<&String>,
    too_large: &str,
    invalid: &str,
    max_len: usize,
) -> Result<(), String> {
    let Some(value) = value else { return Ok(()) };
    if js_len(value) > max_len {
        return Err(too_large.into());
    }
    match serde_json::from_str::<Value>(value) {
        Ok(Value::Object(_)) | Ok(Value::Array(_)) => Ok(()),
        _ => Err(invalid.into()),
    }
}

/// The TS `validateEmailInput`: limits and header-injection checks before any HTTP.
pub fn validate_email_input(a: &SendEmailArgs) -> Result<(), String> {
    if a.to.is_empty() {
        return Err(errors::SEND_EMAIL_NO_RECIPIENTS.into());
    }
    validate_address_list(Some(&a.to), errors::TO_TOO_MANY, errors::TO_INVALID_ADDRESS)?;
    validate_address_list(
        a.cc.as_ref(),
        errors::CC_TOO_MANY,
        errors::CC_INVALID_ADDRESS,
    )?;
    validate_address_list(
        a.bcc.as_ref(),
        errors::BCC_TOO_MANY,
        errors::BCC_INVALID_ADDRESS,
    )?;
    validate_address_list(
        a.reply_to.as_ref(),
        errors::REPLY_TO_TOO_MANY,
        errors::REPLY_TO_INVALID_ADDRESS,
    )?;
    let recipients =
        a.to.len() + a.cc.as_ref().map_or(0, Vec::len) + a.bcc.as_ref().map_or(0, Vec::len);
    if recipients > 100 {
        return Err(errors::SEND_EMAIL_TOO_MANY_RECIPIENTS.into());
    }
    if let Some(from) = &a.from
        && (from.is_empty() || js_len(from) > 320 || has_control_character(from))
    {
        return Err(errors::SEND_EMAIL_INVALID_FROM.into());
    }
    if a.subject.is_empty() || js_len(&a.subject) > 998 || has_control_character(&a.subject) {
        return Err(errors::SEND_EMAIL_INVALID_SUBJECT.into());
    }
    if a.html.is_none() && a.text.is_none() {
        return Err(errors::SEND_EMAIL_MISSING_CONTENT.into());
    }
    if a.html.as_deref().map_or(0, js_len) > 200_000 {
        return Err(errors::SEND_EMAIL_HTML_TOO_LARGE.into());
    }
    if a.text.as_deref().map_or(0, js_len) > 200_000 {
        return Err(errors::SEND_EMAIL_TEXT_TOO_LARGE.into());
    }
    validate_json(
        a.tags_json.as_ref(),
        errors::TAGS_TOO_LARGE,
        errors::TAGS_INVALID_JSON,
        16_384,
    )?;
    validate_json(
        a.headers_json.as_ref(),
        errors::HEADERS_TOO_LARGE,
        errors::HEADERS_INVALID_JSON,
        16_384,
    )?;
    let scheduled = a.scheduled_at.as_deref().unwrap_or("");
    if js_len(scheduled) > 128 || has_control_character(scheduled) {
        return Err(errors::SEND_EMAIL_INVALID_SCHEDULE.into());
    }
    Ok(())
}

/// A validated request to `RESEND_API_BASE` (the TS `buildResendHttpRequest`).
#[derive(Clone, Debug)]
pub struct ResendHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

pub fn build_resend_http_request(
    method: &str,
    path: &str,
    api_key: &str,
    json_body: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<ResendHttpRequest, String> {
    let method = method.trim().to_uppercase();
    if !["GET", "POST", "PATCH", "DELETE"].contains(&method.as_str()) {
        return Err(errors::REQUEST_METHOD_INVALID.into());
    }
    let path = path.trim();
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.contains('#')
        || js_len(path) > MAX_PATH_LENGTH
        || has_control_character(path)
    {
        return Err(errors::REQUEST_PATH_INVALID.into());
    }
    let body = json_body.filter(|b| !b.is_empty());
    if body.is_some_and(|b| js_len(b) > MAX_JSON_BODY_LENGTH) {
        return Err(errors::REQUEST_BODY_TOO_LARGE.into());
    }
    if idempotency_key.is_some_and(|k| js_len(k) > MAX_IDEMPOTENCY_KEY_LENGTH) {
        return Err(errors::IDEMPOTENCY_KEY_TOO_LONG.into());
    }
    let mut headers = vec![
        ("Authorization".to_string(), format!("Bearer {api_key}")),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    if body.is_some() {
        headers.push(("Content-Type".into(), "application/json".into()));
    }
    if let Some(k) = idempotency_key.filter(|k| !k.is_empty()) {
        headers.push(("Idempotency-Key".into(), k.to_string()));
    }
    Ok(ResendHttpRequest {
        method,
        url: format!("{RESEND_API_BASE}{path}"),
        headers,
        body: body.map(str::to_string),
    })
}

fn call_resend(
    ctx: &mut ProcedureContext,
    method: &str,
    path: &str,
    api_key: &str,
    json_body: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<ResendApiRequestResult, String> {
    let r = build_resend_http_request(method, path, api_key, json_body, idempotency_key)?;
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
    let body = response.into_body().into_string_lossy();
    Ok(ResendApiRequestResult { status, body })
}

/// `resend.<op>_failed:<status>[:name=…|msg=…]` unless the response is 2xx.
fn ensure_ok(response: &ResendApiRequestResult, prefix: &str) -> Result<(), String> {
    if (200..300).contains(&response.status) {
        return Ok(());
    }
    Err(format!(
        "{prefix}:{}{}",
        response.status,
        resend_error_suffix(&response.body)
    ))
}

fn resend_error_suffix(body: &str) -> String {
    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(body) {
        let name = o.get("name");
        let message = o.get("message");
        let status_code = o.get("statusCode");
        let shape_ok = name.is_none_or(Value::is_string)
            && message.is_none_or(Value::is_string)
            && status_code.is_none_or(|v| v.is_number() || v.is_null());
        if shape_ok {
            let mut parts = Vec::new();
            if let Some(n) = name.and_then(Value::as_str).filter(|n| !n.is_empty()) {
                parts.push(format!("name={n}"));
            }
            if let Some(m) = message.and_then(Value::as_str).filter(|m| !m.is_empty()) {
                parts.push(format!("msg={}", js_slice(&collapse_whitespace(m), 240)));
            }
            if !parts.is_empty() {
                return format!(":{}", parts.join("|"));
            }
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

// ---------------------------------------------------------------------------
// Email rows
// ---------------------------------------------------------------------------

/// Sparse fields for [`upsert_email`]: `None` keeps the stored value.
#[derive(Clone, Debug, Default)]
struct EmailUpsert {
    resend_id: String,
    from_address: String,
    to_addresses_json: String,
    subject: Option<String>,
    html: Option<String>,
    text: Option<String>,
    status: Option<ResendEmailStatus>,
    status_updated_at: Option<Timestamp>,
    last_error: Option<String>,
    bounced_at: Option<Timestamp>,
    bounce_json: Option<String>,
    failed_at: Option<Timestamp>,
    failure_reason: Option<String>,
    complained: bool,
    complained_at: Option<Timestamp>,
    opened: bool,
    opened_at: Option<Timestamp>,
    clicked: bool,
    clicked_at: Option<Timestamp>,
    delivered_at: Option<Timestamp>,
    sent_at: Option<Timestamp>,
    tags_json: Option<String>,
    user_id: Option<String>,
    org_id: Option<String>,
}

fn upsert_email(ctx: &ReducerContext, now: Timestamp, a: EmailUpsert) {
    let existing = ctx.db.resend_email().resend_id().find(&a.resend_id);
    let status_time = a.status_updated_at.unwrap_or(now);
    // Late webhooks and send responses can't undo a later delivery stage.
    let replace = match (a.status, &existing) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(s), Some(e)) => {
            s.order() >= e.status.order()
                && (e.status == ResendEmailStatus::Queued
                    || e.status_updated_at.is_none_or(|t| {
                        status_time.to_micros_since_unix_epoch() >= t.to_micros_since_unix_epoch()
                    }))
        }
    };
    let e = existing.as_ref();
    // Error and bounce details describe the status that produced them.
    let detail = |v: &Option<String>, old: Option<&Option<String>>| -> Option<String> {
        if replace && v.is_some() {
            v.clone()
        } else {
            old.cloned().flatten()
        }
    };
    let detail_t = |v: Option<Timestamp>, old: Option<Option<Timestamp>>| -> Option<Timestamp> {
        if replace && v.is_some() {
            v
        } else {
            old.flatten()
        }
    };
    let row = ResendEmail {
        resend_id: a.resend_id.clone(),
        from_address: a.from_address.clone(),
        to_addresses_json: a.to_addresses_json.clone(),
        subject: a
            .subject
            .clone()
            .or_else(|| e.and_then(|e| e.subject.clone())),
        html: a.html.clone().or_else(|| e.and_then(|e| e.html.clone())),
        text: a.text.clone().or_else(|| e.and_then(|e| e.text.clone())),
        status: if replace {
            a.status.unwrap()
        } else {
            e.map_or(ResendEmailStatus::Queued, |e| e.status)
        },
        status_updated_at: if replace {
            Some(status_time)
        } else {
            e.and_then(|e| e.status_updated_at)
        },
        last_error: detail(&a.last_error, e.map(|e| &e.last_error)),
        bounced_at: detail_t(a.bounced_at, e.map(|e| e.bounced_at)),
        bounce_json: detail(&a.bounce_json, e.map(|e| &e.bounce_json)),
        failed_at: detail_t(a.failed_at, e.map(|e| e.failed_at)),
        failure_reason: detail(&a.failure_reason, e.map(|e| &e.failure_reason)),
        complained: a.complained || e.is_some_and(|e| e.complained),
        complained_at: a.complained_at.or_else(|| e.and_then(|e| e.complained_at)),
        opened: a.opened || e.is_some_and(|e| e.opened),
        opened_at: a.opened_at.or_else(|| e.and_then(|e| e.opened_at)),
        clicked: a.clicked || e.is_some_and(|e| e.clicked),
        clicked_at: a.clicked_at.or_else(|| e.and_then(|e| e.clicked_at)),
        delivered_at: a.delivered_at.or_else(|| e.and_then(|e| e.delivered_at)),
        sent_at: a.sent_at.or_else(|| e.and_then(|e| e.sent_at)),
        tags_json: a
            .tags_json
            .clone()
            .or_else(|| e.and_then(|e| e.tags_json.clone())),
        user_id: a
            .user_id
            .clone()
            .or_else(|| e.and_then(|e| e.user_id.clone())),
        org_id: a
            .org_id
            .clone()
            .or_else(|| e.and_then(|e| e.org_id.clone())),
        created_at: e.map_or(now, |e| e.created_at),
        updated_at: now,
    };
    if existing.is_some() {
        ctx.db.resend_email().resend_id().update(row);
    } else {
        ctx.db.resend_email().insert(row);
    }
}

/// `userId` / `orgId` from send-API tags (`[{ "name": "userId", "value": "u_1" }]`).
fn tag_fields_from_send_tags(tags_json: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(Ok(Value::Array(items))) = tags_json.map(serde_json::from_str::<Value>) else {
        return (None, None);
    };
    let mut pairs = Vec::new();
    for item in &items {
        let (Some(name), Some(value)) = (
            item.get("name").and_then(Value::as_str),
            item.get("value").and_then(Value::as_str),
        ) else {
            return (None, None);
        };
        if !item.is_object() {
            return (None, None);
        }
        pairs.push((name.to_string(), value.to_string()));
    }
    let find = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    (find("userId"), find("orgId"))
}

fn build_send_email_body(a: &SendEmailArgs, from: &str) -> String {
    let mut body = Map::new();
    body.insert("from".into(), json!(from));
    body.insert("to".into(), json!(a.to));
    body.insert("subject".into(), json!(a.subject));
    if let Some(h) = &a.html {
        body.insert("html".into(), json!(h));
    }
    if let Some(t) = &a.text {
        body.insert("text".into(), json!(t));
    }
    if let Some(cc) = a.cc.as_ref().filter(|v| !v.is_empty()) {
        body.insert("cc".into(), json!(cc));
    }
    if let Some(bcc) = a.bcc.as_ref().filter(|v| !v.is_empty()) {
        body.insert("bcc".into(), json!(bcc));
    }
    if let Some(r) = a.reply_to.as_ref().filter(|v| !v.is_empty()) {
        body.insert("reply_to".into(), json!(r));
    }
    if let Some(s) = &a.scheduled_at {
        body.insert("scheduled_at".into(), json!(s));
    }
    if let Some(Ok(tags)) = a.tags_json.as_deref().map(serde_json::from_str::<Value>) {
        body.insert("tags".into(), tags);
    }
    if let Some(Ok(headers)) = a.headers_json.as_deref().map(serde_json::from_str::<Value>) {
        body.insert("headers".into(), headers);
    }
    js_json(&Value::Object(body))
}

/// Send an email (no admin check: call it from your own procedure after deciding who may send
/// to whom). Records a `Queued` row on success.
pub fn send_email_request(
    ctx: &mut ProcedureContext,
    args: SendEmailArgs,
) -> Result<ResendSendEmailResult, String> {
    validate_email_input(&args)?;
    let cfg = ctx.with_tx(|tx| load_config(tx))?;
    let from = args
        .from
        .clone()
        .or(cfg.default_from.clone())
        .ok_or_else(|| errors::SEND_EMAIL_MISSING_FROM.to_string())?;
    let body = build_send_email_body(&args, &from);
    let response = call_resend(
        ctx,
        "POST",
        "/emails",
        &cfg.api_key,
        Some(&body),
        args.idempotency_key.as_deref(),
    )?;
    ensure_ok(&response, errors::SEND_EMAIL_FAILED)?;
    let parsed: Value = serde_json::from_str(&response.body)
        .map_err(|_| errors::SEND_EMAIL_INVALID_RESPONSE.to_string())?;
    let resend_id = match parsed.get("id") {
        _ if !parsed.is_object() => {
            return Err(format!(
                "{}:Invalid type: Expected Object",
                errors::SEND_EMAIL_INVALID_RESPONSE
            ));
        }
        Some(Value::String(id)) => id.clone(),
        Some(_) => {
            return Err(format!(
                "{}:Invalid type: Expected string at id",
                errors::SEND_EMAIL_INVALID_RESPONSE
            ));
        }
        None => {
            return Err(format!(
                "{}:Invalid key: Expected \"id\" but received undefined at id",
                errors::SEND_EMAIL_INVALID_RESPONSE
            ));
        }
    };
    let now = ctx.timestamp;
    let (user_id, org_id) = tag_fields_from_send_tags(args.tags_json.as_deref());
    let to_json = json!(args.to).to_string();
    ctx.with_tx(|tx| {
        upsert_email(
            tx,
            now,
            EmailUpsert {
                resend_id: resend_id.clone(),
                from_address: from.clone(),
                to_addresses_json: to_json.clone(),
                subject: Some(args.subject.clone()),
                html: args.html.clone(),
                text: args.text.clone(),
                status: Some(ResendEmailStatus::Queued),
                tags_json: args.tags_json.clone(),
                user_id: user_id.clone(),
                org_id: org_id.clone(),
                ..Default::default()
            },
        )
    });
    Ok(ResendSendEmailResult { resend_id })
}

/// Admins only.
#[spacetimedb::procedure]
#[allow(clippy::too_many_arguments)]
pub fn resend_send_email(
    ctx: &mut ProcedureContext,
    from: Option<String>,
    to: Vec<String>,
    subject: String,
    html: Option<String>,
    text: Option<String>,
    cc: Option<Vec<String>>,
    bcc: Option<Vec<String>>,
    reply_to: Option<Vec<String>>,
    tags_json: Option<String>,
    headers_json: Option<String>,
    scheduled_at: Option<String>,
    idempotency_key: Option<String>,
) -> Result<ResendSendEmailResult, String> {
    require_procedure_admin(ctx)?;
    send_email_request(
        ctx,
        SendEmailArgs {
            from,
            to,
            subject,
            html,
            text,
            cc,
            bcc,
            reply_to,
            tags_json,
            headers_json,
            scheduled_at,
            idempotency_key,
        },
    )
}

/// Admins only.
#[spacetimedb::procedure]
pub fn resend_cancel_email(ctx: &mut ProcedureContext, resend_id: String) -> Result<(), String> {
    require_procedure_admin(ctx)?;
    let cfg = ctx.with_tx(|tx| load_config(tx))?;
    let response = call_resend(
        ctx,
        "POST",
        &format!("/emails/{resend_id}/cancel"),
        &cfg.api_key,
        None,
        None,
    )?;
    ensure_ok(&response, errors::CANCEL_EMAIL_FAILED)?;
    let now = ctx.timestamp;
    ctx.with_tx(|tx| {
        if let Some(mut e) = tx.db.resend_email().resend_id().find(&resend_id) {
            e.status = ResendEmailStatus::Cancelled;
            e.status_updated_at = Some(now);
            e.updated_at = now;
            tx.db.resend_email().resend_id().update(e);
        }
    });
    Ok(())
}

/// Admins only: an authenticated request to a relative `api.resend.com` path.
#[spacetimedb::procedure]
pub fn resend_api_request(
    ctx: &mut ProcedureContext,
    method: String,
    path: String,
    json_body: Option<String>,
    idempotency_key: Option<String>,
) -> Result<ResendApiRequestResult, String> {
    require_procedure_admin(ctx)?;
    let cfg = ctx.with_tx(|tx| load_config(tx))?;
    call_resend(
        ctx,
        &method,
        &path,
        &cfg.api_key,
        json_body.as_deref(),
        idempotency_key.as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Admin queries (at most 1,000 rows each)
// ---------------------------------------------------------------------------

#[spacetimedb::procedure]
pub fn resend_get_email(
    ctx: &mut ProcedureContext,
    resend_id: String,
) -> Result<Option<ResendEmail>, String> {
    require_procedure_admin(ctx)?;
    Ok(ctx.with_tx(|tx| tx.db.resend_email().resend_id().find(&resend_id)))
}

#[spacetimedb::procedure]
pub fn resend_get_webhook_event(
    ctx: &mut ProcedureContext,
    event_id: String,
) -> Result<Option<ResendWebhookEvent>, String> {
    require_procedure_admin(ctx)?;
    Ok(ctx.with_tx(|tx| tx.db.resend_webhook_event().event_id().find(&event_id)))
}

#[spacetimedb::procedure]
pub fn resend_list_emails_by_user_id(
    ctx: &mut ProcedureContext,
    user_id: String,
) -> Result<Vec<ResendEmail>, String> {
    require_procedure_admin(ctx)?;
    // Rust can't filter an index on an `Option` column, so this scans (TS uses `byUserId`).
    let key = Some(user_id);
    Ok(ctx.with_tx(|tx| {
        tx.db
            .resend_email()
            .iter()
            .filter(|e| e.user_id == key)
            .take(MAX_QUERY_ROWS)
            .collect()
    }))
}

#[spacetimedb::procedure]
pub fn resend_list_emails_by_org_id(
    ctx: &mut ProcedureContext,
    org_id: String,
) -> Result<Vec<ResendEmail>, String> {
    require_procedure_admin(ctx)?;
    let key = Some(org_id);
    Ok(ctx.with_tx(|tx| {
        tx.db
            .resend_email()
            .iter()
            .filter(|e| e.org_id == key)
            .take(MAX_QUERY_ROWS)
            .collect()
    }))
}

#[spacetimedb::procedure]
pub fn resend_list_emails_by_status(
    ctx: &mut ProcedureContext,
    status: ResendEmailStatus,
) -> Result<Vec<ResendEmail>, String> {
    require_procedure_admin(ctx)?;
    Ok(ctx.with_tx(|tx| {
        tx.db
            .resend_email()
            .status()
            .filter(&status)
            .take(MAX_QUERY_ROWS)
            .collect()
    }))
}

#[spacetimedb::procedure]
pub fn resend_list_delivery_events_for_email(
    ctx: &mut ProcedureContext,
    resend_id: String,
) -> Result<Vec<ResendDeliveryEvent>, String> {
    require_procedure_admin(ctx)?;
    Ok(ctx.with_tx(|tx| {
        tx.db
            .resend_delivery_event()
            .resend_id()
            .filter(&resend_id)
            .take(MAX_QUERY_ROWS)
            .collect()
    }))
}

// ---------------------------------------------------------------------------
// Webhooks
// ---------------------------------------------------------------------------

const HANDLED_EVENT_TYPES: [&str; 8] = [
    "email.sent",
    "email.delivered",
    "email.delivery_delayed",
    "email.complained",
    "email.bounced",
    "email.opened",
    "email.clicked",
    "email.failed",
];

/// `type` from a webhook payload, if it's a non-empty string in a JSON object.
pub fn parse_resend_event_type(payload_json: &str) -> Option<String> {
    match serde_json::from_str::<Value>(payload_json) {
        Ok(Value::Object(o)) => match o.get("type") {
            Some(Value::String(t)) if !t.is_empty() => Some(t.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// A validated `email.*` event (the TS valibot `vEmailEvent`).
struct EmailEvent {
    kind: &'static str,
    created_at: String,
    email_id: String,
    from: String,
    to: Vec<String>,
    subject: String,
    tags: Option<Map<String, Value>>,
    /// `bounce`, `failed` or `click`, re-serialized with only the schema's keys.
    detail: Option<Value>,
    bounce_message: Option<String>,
    failed_reason: Option<String>,
}

fn want_str(o: &Map<String, Value>, key: &str, path: &str) -> Result<String, String> {
    match o.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("Invalid type: Expected string at {path}{key}")),
        None => Err(format!(
            "Invalid key: Expected \"{key}\" but received undefined at {path}{key}"
        )),
    }
}

fn opt_str(o: &Map<String, Value>, key: &str, path: &str) -> Result<(), String> {
    match o.get(key) {
        None | Some(Value::String(_)) => Ok(()),
        Some(_) => Err(format!("Invalid type: Expected string at {path}{key}")),
    }
}

fn want_obj<'a>(
    o: &'a Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<&'a Map<String, Value>, String> {
    match o.get(key) {
        Some(Value::Object(m)) => Ok(m),
        Some(_) => Err(format!("Invalid type: Expected Object at {path}{key}")),
        None => Err(format!(
            "Invalid key: Expected \"{key}\" but received undefined at {path}{key}"
        )),
    }
}

/// Copy of `keys` from a nested object, in schema order, all required strings.
fn sub_object(data: &Map<String, Value>, key: &str, keys: &[&str]) -> Result<Value, String> {
    let o = want_obj(data, key, "data.")?;
    let mut out = Map::new();
    for k in keys {
        out.insert(
            (*k).into(),
            Value::String(want_str(o, k, &format!("data.{key}."))?),
        );
    }
    Ok(Value::Object(out))
}

fn parse_email_event(v: &Value) -> Result<EmailEvent, String> {
    let Value::Object(o) = v else {
        return Err("Invalid type: Expected Object".into());
    };
    let kind = match o.get("type").and_then(Value::as_str) {
        Some(t) => HANDLED_EVENT_TYPES
            .iter()
            .find(|h| **h == t)
            .copied()
            .ok_or_else(|| "Invalid type at type".to_string())?,
        None => return Err("Invalid type at type".into()),
    };
    let created_at = want_str(o, "created_at", "")?;
    let data = want_obj(o, "data", "")?;
    opt_str(data, "broadcast_id", "data.")?;
    let data_created_at = want_str(data, "created_at", "data.")?;
    let _ = data_created_at;
    let email_id = want_str(data, "email_id", "data.")?;
    let from = want_str(data, "from", "data.")?;
    let to = match data.get("to") {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::String(s) => out.push(s.clone()),
                    _ => return Err(format!("Invalid type: Expected string at data.to.{i}")),
                }
            }
            out
        }
        Some(_) => return Err("Invalid type: Expected Array at data.to".into()),
        None => {
            return Err("Invalid key: Expected \"to\" but received undefined at data.to".into());
        }
    };
    let subject = want_str(data, "subject", "data.")?;
    opt_str(data, "template_id", "data.")?;
    let tags = match data.get("tags") {
        None => None,
        Some(Value::Object(m)) => {
            for (k, v) in m {
                if !v.is_string() {
                    return Err(format!("Invalid type: Expected string at data.tags.{k}"));
                }
            }
            Some(m.clone())
        }
        Some(_) => return Err("Invalid type: Expected Object at data.tags".into()),
    };
    let mut ev = EmailEvent {
        kind,
        created_at,
        email_id,
        from,
        to,
        subject,
        tags,
        detail: None,
        bounce_message: None,
        failed_reason: None,
    };
    match kind {
        "email.bounced" => {
            let d = sub_object(data, "bounce", &["message", "subType", "type"])?;
            ev.bounce_message = d["message"].as_str().map(str::to_string);
            ev.detail = Some(d);
        }
        "email.clicked" => {
            ev.detail = Some(sub_object(
                data,
                "click",
                &["ipAddress", "link", "timestamp", "userAgent"],
            )?);
        }
        "email.failed" => {
            let d = sub_object(data, "failed", &["reason"])?;
            ev.failed_reason = d["reason"].as_str().map(str::to_string);
            ev.detail = Some(d);
        }
        _ => {}
    }
    Ok(ev)
}

/// Milliseconds since the Unix epoch for ISO 8601 / RFC 3339 dates (`Date.parse`'s main form).
/// Date-only and offset-less date-times are UTC.
pub fn parse_iso8601_millis(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = b.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0i64, |n, d| n * 10 + (d - b'0') as i64))
    };
    let (year, mut i) = match b.first()? {
        b'+' | b'-' => {
            let y = num(1, 6)?;
            (if b[0] == b'-' { -y } else { y }, 7)
        }
        _ => (num(0, 4)?, 4),
    };
    let (mut month, mut day) = (1, 1);
    if b.get(i) == Some(&b'-') {
        month = num(i + 1, 2)?;
        i += 3;
        if b.get(i) == Some(&b'-') {
            day = num(i + 1, 2)?;
            i += 3;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut h, mut mi, mut sec, mut ms) = (0, 0, 0, 0);
    let mut offset_min = 0;
    if i < b.len() {
        if b[i] != b'T' && b[i] != b't' && b[i] != b' ' {
            return None;
        }
        h = num(i + 1, 2)?;
        if b.get(i + 3) != Some(&b':') {
            return None;
        }
        mi = num(i + 4, 2)?;
        i += 6;
        if b.get(i) == Some(&b':') {
            sec = num(i + 1, 2)?;
            i += 3;
            if b.get(i) == Some(&b'.') || b.get(i) == Some(&b',') {
                let start = i + 1;
                let mut end = start;
                while end < b.len() && b[end].is_ascii_digit() {
                    end += 1;
                }
                if end == start {
                    return None;
                }
                let frac = &s[start..end.min(start + 3)];
                ms = frac.parse::<i64>().ok()? * 10i64.pow(3 - frac.len() as u32);
                i = end;
            }
        }
        match b.get(i) {
            None => {}
            Some(b'Z') | Some(b'z') if i + 1 == b.len() => {}
            Some(c @ (b'+' | b'-')) => {
                let oh = num(i + 1, 2)?;
                let om = if b.get(i + 3) == Some(&b':') {
                    num(i + 4, 2)?
                } else {
                    num(i + 3, 2)?
                };
                let end = if b.get(i + 3) == Some(&b':') {
                    i + 6
                } else {
                    i + 5
                };
                if end != b.len() {
                    return None;
                }
                offset_min = (oh * 60 + om) * if *c == b'-' { -1 } else { 1 };
            }
            _ => return None,
        }
        if h > 24 || mi > 59 || sec > 59 || (h == 24 && (mi, sec, ms) != (0, 0, 0)) {
            return None;
        }
    }
    // Days from civil (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi - offset_min) * 60_000 + sec * 1000 + ms)
}

fn record_delivery_event(ctx: &ReducerContext, now: Timestamp, ev: ResendDeliveryEvent) {
    if ctx
        .db
        .resend_delivery_event()
        .event_id()
        .find(&ev.event_id)
        .is_some()
    {
        return;
    }
    ctx.db.resend_delivery_event().insert(ResendDeliveryEvent {
        inserted_at: now,
        ..ev
    });
}

fn update_webhook_status(
    ctx: &ReducerContext,
    event_id: &str,
    status: ResendWebhookEventStatus,
    error: Option<String>,
) {
    let Some(mut row) = ctx
        .db
        .resend_webhook_event()
        .event_id()
        .find(event_id.to_string())
    else {
        return;
    };
    if status != ResendWebhookEventStatus::Received {
        row.processed_at = Some(ctx.timestamp);
    }
    row.status = status;
    row.error_message = error;
    ctx.db.resend_webhook_event().event_id().update(row);
}

/// Apply a stored or incoming payload to `resend_email` / `resend_delivery_event`.
fn apply_resend_event(
    ctx: &ReducerContext,
    event_id: &str,
    payload_json: &str,
) -> (ResendWebhookEventStatus, Option<String>) {
    use ResendWebhookEventStatus as S;
    let Ok(parsed) = serde_json::from_str::<Value>(payload_json) else {
        return (S::Failed, Some("invalid JSON payload".into()));
    };
    let ev = match parse_email_event(&parsed) {
        Ok(ev) => ev,
        Err(issue) => {
            let handled = parsed
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| HANDLED_EVENT_TYPES.contains(&t));
            if !handled {
                return (S::Ignored, None);
            }
            return (
                S::Failed,
                Some(format!("payload validation failed: {issue}")),
            );
        }
    };
    let Some(millis) = parse_iso8601_millis(&ev.created_at) else {
        return (S::Failed, Some("invalid event timestamp".into()));
    };
    let event_time = Timestamp::from_micros_since_unix_epoch(millis * 1000);
    let tags_json = ev.tags.as_ref().map(|t| js_json(&Value::Object(t.clone())));
    let tag = |k: &str| {
        ev.tags
            .as_ref()
            .and_then(|t| t.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let mut a = EmailUpsert {
        resend_id: ev.email_id.clone(),
        from_address: ev.from.clone(),
        to_addresses_json: json!(ev.to).to_string(),
        subject: Some(ev.subject.clone()),
        tags_json,
        user_id: tag("userId"),
        org_id: tag("orgId"),
        status_updated_at: Some(event_time),
        ..Default::default()
    };
    match ev.kind {
        "email.sent" => {
            a.status = Some(ResendEmailStatus::Sent);
            a.sent_at = Some(event_time);
        }
        "email.delivered" => {
            a.status = Some(ResendEmailStatus::Delivered);
            a.delivered_at = Some(event_time);
        }
        "email.delivery_delayed" => a.status = Some(ResendEmailStatus::DeliveryDelayed),
        "email.bounced" => {
            a.status = Some(ResendEmailStatus::Bounced);
            a.bounced_at = Some(event_time);
            a.bounce_json = ev.detail.as_ref().map(Value::to_string);
            a.last_error = ev.bounce_message.clone();
        }
        "email.failed" => {
            a.status = Some(ResendEmailStatus::Failed);
            a.failed_at = Some(event_time);
            a.failure_reason = ev.failed_reason.clone();
            a.last_error = ev.failed_reason.clone();
        }
        "email.complained" => {
            a.complained = true;
            a.complained_at = Some(event_time);
        }
        "email.opened" => {
            a.opened = true;
            a.opened_at = Some(event_time);
        }
        "email.clicked" => {
            a.clicked = true;
            a.clicked_at = Some(event_time);
        }
        _ => {}
    }
    let now = ctx.timestamp;
    upsert_email(ctx, now, a);
    record_delivery_event(
        ctx,
        now,
        ResendDeliveryEvent {
            event_id: event_id.to_string(),
            resend_id: ev.email_id.clone(),
            event_type: ev.kind.to_string(),
            created_at_iso: ev.created_at.clone(),
            detail_json: ev.detail.as_ref().map(Value::to_string),
            inserted_at: now,
        },
    );
    (S::Processed, None)
}

/// Why a webhook was refused before anything was written: an HTTP status and a code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookRejection {
    pub status: u16,
    pub code: String,
}

/// Verify, store and apply one webhook in the caller's transaction (the reducer and the HTTP
/// route share this). Returns the stored status, or a rejection when nothing was written.
pub fn apply_resend_webhook(
    ctx: &ReducerContext,
    event_id: &str,
    payload_json: &str,
    signature_header: Option<&str>,
    timestamp_header: Option<&str>,
) -> Result<ResendWebhookEventStatus, WebhookRejection> {
    let reject = |status: u16, code: &str| {
        Err(WebhookRejection {
            status,
            code: code.to_string(),
        })
    };
    if event_id.is_empty() || js_len(event_id) > MAX_WEBHOOK_METADATA_LENGTH {
        return reject(400, errors::WEBHOOK_METADATA_INVALID);
    }
    if js_len(payload_json) > MAX_WEBHOOK_BODY_LENGTH {
        return reject(413, errors::WEBHOOK_PAYLOAD_TOO_LARGE);
    }
    if signature_header.map_or(0, js_len) > MAX_WEBHOOK_HEADER_LENGTH
        || timestamp_header.map_or(0, js_len) > MAX_WEBHOOK_HEADER_LENGTH
    {
        return reject(400, errors::WEBHOOK_HEADER_TOO_LARGE);
    }
    let Some(secret) = ctx
        .db
        .resend_config()
        .singleton()
        .find(true)
        .and_then(|c| c.webhook_signing_secret)
        .filter(|s| !s.is_empty())
    else {
        return reject(500, errors::WEBHOOK_SECRET_NOT_CONFIGURED);
    };
    let verified = crypto::verify_svix_signature(&crypto::SvixVerifyOpts {
        raw_body: payload_json,
        svix_id: event_id,
        svix_timestamp: timestamp_header.unwrap_or(""),
        svix_signature: signature_header.unwrap_or(""),
        secret: &secret,
        tolerance_seconds: Some(crypto::DEFAULT_TOLERANCE_SECONDS),
        now_seconds: ctx
            .timestamp
            .to_micros_since_unix_epoch()
            .div_euclid(1_000_000),
    });
    if let Err(reason) = verified {
        return reject(
            401,
            &format!("{}:{}", errors::WEBHOOK_SIGNATURE_MISMATCH, reason.code()),
        );
    }
    let Some(event_type) =
        parse_resend_event_type(payload_json).filter(|t| js_len(t) <= MAX_WEBHOOK_METADATA_LENGTH)
    else {
        return reject(400, errors::WEBHOOK_PAYLOAD_INVALID);
    };
    let existing = ctx
        .db
        .resend_webhook_event()
        .event_id()
        .find(event_id.to_string());
    if let Some(e) = &existing
        && matches!(
            e.status,
            ResendWebhookEventStatus::Processed | ResendWebhookEventStatus::Ignored
        )
    {
        return Ok(e.status);
    }
    if existing.is_none() {
        ctx.db.resend_webhook_event().insert(ResendWebhookEvent {
            event_id: event_id.to_string(),
            event_type,
            payload_json: payload_json.to_string(),
            signature_header: signature_header.map(str::to_string),
            timestamp_header: timestamp_header.map(str::to_string),
            status: ResendWebhookEventStatus::Received,
            error_message: None,
            received_at: ctx.timestamp,
            processed_at: None,
        });
    }
    let payload = existing
        .as_ref()
        .map_or(payload_json, |e| e.payload_json.as_str());
    let (status, error) = apply_resend_event(ctx, event_id, payload);
    update_webhook_status(ctx, event_id, status, error);
    Ok(status)
}

/// Ingest a signed webhook (Svix headers passed as arguments). A `Failed` row commits so it
/// can be read and replayed; rejections write nothing.
#[spacetimedb::reducer]
pub fn ingest_resend_webhook(
    ctx: &ReducerContext,
    event_id: String,
    event_type: String,
    payload_json: String,
    signature_header: Option<String>,
    timestamp_header: Option<String>,
) -> Result<(), String> {
    if parse_resend_event_type(&payload_json).as_deref() != Some(event_type.as_str()) {
        return Err(errors::WEBHOOK_METADATA_MISMATCH.into());
    }
    apply_resend_webhook(
        ctx,
        &event_id,
        &payload_json,
        signature_header.as_deref(),
        timestamp_header.as_deref(),
    )
    .map(|_| ())
    .map_err(|r| r.code)
}

/// Admins only: re-apply a stored event.
#[spacetimedb::reducer]
pub fn resend_replay_webhook_event(ctx: &ReducerContext, event_id: String) -> Result<(), String> {
    if !is_admin(ctx, ctx.sender()) {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    let event = ctx
        .db
        .resend_webhook_event()
        .event_id()
        .find(&event_id)
        .ok_or_else(|| format!("{}:{event_id}", errors::WEBHOOK_EVENT_NOT_FOUND))?;
    let (status, error) = apply_resend_event(ctx, &event_id, &event.payload_json);
    update_webhook_status(ctx, &event_id, status, error);
    Ok(())
}

fn webhook_json(status: u16, ok: bool, code: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from_bytes(
            json!({ "ok": ok, "code": code }).to_string(),
        ))
        .expect("valid response")
}

/// HTTP route for Resend's webhooks (the TS `makeResendWebhookHandler()`): register it on your
/// router, e.g. `.post("/webhook/resend", resend::resend_webhook)`.
#[handler]
pub fn resend_webhook(ctx: &mut HandlerContext, req: Request) -> Response {
    if req.method().as_str().to_uppercase() != "POST" {
        return webhook_json(405, false, errors::WEBHOOK_METHOD_NOT_ALLOWED);
    }
    let header = |name: &str| -> Option<String> {
        let values: Vec<String> = req
            .headers()
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();
        (!values.is_empty()).then(|| values.join(", "))
    };
    let event_id = header("svix-id").unwrap_or_default();
    let signature = header("svix-signature");
    let timestamp = header("svix-timestamp");
    let payload = req.into_body().into_string_lossy();
    let result = ctx.with_tx(|tx| {
        apply_resend_webhook(
            tx,
            &event_id,
            &payload,
            signature.as_deref(),
            timestamp.as_deref(),
        )
    });
    match result {
        Err(r) => webhook_json(r.status, false, &r.code),
        Ok(ResendWebhookEventStatus::Failed) => {
            webhook_json(400, false, errors::WEBHOOK_PAYLOAD_INVALID)
        }
        Ok(_) => webhook_json(200, true, "ok"),
    }
}
