//! # posthog.rs: PostHog analytics (a durable event outbox) and feature flags, for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/posthog` (SpacetimeDB PR #5907, commit
//! `b1063b1876f4d7b8c723adb54b3d0f4d248b9eb0`, package `spacetime-posthog-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Queue events in the same transaction as your own changes; a scheduled procedure sends them to
//! PostHog's `/batch` endpoint every 5 seconds with retries and backoff, and admins can capture
//! directly, evaluate feature flags, requeue failures and inspect delivery. Same tables, views,
//! error codes, limits and retry rules as the TS package. Needs `serde_json` with
//! `preserve_order`.
//!
//! ```ignore
//! #[path = "posthog.rs"] pub mod posthog;
//!
//! #[spacetimedb::reducer(init)]
//! pub fn init(ctx: &ReducerContext) { posthog::install(ctx); }
//!
//! #[spacetimedb::reducer]
//! pub fn complete_order(ctx: &ReducerContext, order_id: String) {
//!     // … your order mutation …
//!     posthog::enqueue_event_in_tx(ctx, posthog::EnqueueEventArgs {
//!         distinct_id: ctx.sender().to_hex().to_string(),
//!         event: "order_completed".into(),
//!         properties_json: Some(format!(r#"{{"orderId":"{order_id}"}}"#)),
//!         idempotency_key: Some(format!("order_completed:{order_id}")),
//!     });
//! }
//! ```
//!
//! Changes from the TypeScript original:
//! - Names: generic procedures and reducers get a `posthog_` prefix (`enqueue_event` →
//!   `posthog_enqueue_event`, `capture_now` → `posthog_capture_now`, `flush_outbox`,
//!   `requeue_failed_events`, `clear_analytics`, `get_feature_flag`, `scheduled_flush`,
//!   `add_admin_identity`, `remove_admin_identity` likewise). Tables, views and types already
//!   carry the package and are unchanged.
//! - Procedures return `Result<T, String>` instead of throwing; the scheduled flush keeps the
//!   TS unit return and panics for callers other than the scheduler.
//! - JSON sent to PostHog is serialized like JS `JSON.stringify` (integer-like keys first).

use serde_json::{Map, Value};
use spacetimedb::{
    Identity, ProcedureContext, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration,
    Timestamp, ViewContext,
};

pub const FLUSH_INTERVAL_MICROS: i64 = 5 * 1_000_000;
pub const MAX_DELIVERY_ATTEMPTS: u32 = 5;
const INITIAL_RETRY_DELAY_MICROS: i64 = 1_000_000;
const MAX_RETRY_DELAY_MICROS: i64 = 5 * 60 * 1_000_000;
const MAX_LOG_BODY_LENGTH: usize = 2048;
pub const MAX_FLUSH_LIMIT: u32 = 100;
const MAX_FLUSH_BATCHES_PER_TICK: usize = 10;
const CLAIM_TTL_MICROS: i64 = 5 * 60 * 1_000_000;
const MAX_EXPIRED_CLAIMS_PER_FLUSH: usize = 1_000;
const MAX_DISTINCT_ID_LENGTH: usize = 256;
const MAX_EVENT_NAME_LENGTH: usize = 200;
const MAX_PROPERTIES_JSON_LENGTH: usize = 64 * 1024;
const MAX_IDEMPOTENCY_KEY_LENGTH: usize = 256;
const HISTORY_RETENTION_MICROS: i64 = 30 * 24 * 60 * 60 * 1_000_000;
const MAX_RETENTION_ROWS_PER_TICK: usize = 1_000;
const MAX_ADMIN_BATCH: u32 = 10_000;

pub mod errors {
    pub const NOT_AUTHORIZED: &str = "posthog.not_authorized";
    pub const CANNOT_REMOVE_LAST_ADMIN: &str = "posthog.cannot_remove_last_admin";
    pub const CONFIG_MISSING: &str = "posthog.config_missing";
    pub const INVALID_HOST: &str = "posthog.invalid_host";
    pub const INVALID_PROJECT_API_KEY: &str = "posthog.invalid_project_api_key";
    pub const INVALID_DISTINCT_ID: &str = "posthog.invalid_distinct_id";
    pub const INVALID_EVENT: &str = "posthog.invalid_event";
    pub const INVALID_PROPERTIES_JSON: &str = "posthog.invalid_properties_json";
    pub const PROPERTIES_TOO_LARGE: &str = "posthog.properties_too_large";
    pub const IDEMPOTENCY_KEY_TOO_LONG: &str = "posthog.idempotency_key_too_long";
    pub const INVALID_FLAG_KEY: &str = "posthog.invalid_flag_key";
    pub const INVALID_PERSON_PROPERTIES_JSON: &str = "posthog.invalid_person_properties_json";
    pub const PERSON_PROPERTIES_TOO_LARGE: &str = "posthog.person_properties_too_large";
    pub const INVALID_GROUPS_JSON: &str = "posthog.invalid_groups_json";
    pub const GROUPS_TOO_LARGE: &str = "posthog.groups_too_large";
    pub const INVALID_FLUSH_LIMIT: &str = "posthog.invalid_flush_limit";
    pub const INVALID_CLEAR_BATCH: &str = "posthog.invalid_clear_batch";
    pub const INVALID_REQUEUE_LIMIT: &str = "posthog.invalid_requeue_limit";
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostHogOutboxStatus {
    Queued,
    Processing,
    Delivered,
    Failed,
    Rejected,
}
use PostHogOutboxStatus as O;

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostHogDeliverySource {
    Direct,
    Flush,
    FeatureFlag,
}

#[spacetimedb::table(accessor = posthog_config)]
#[derive(Clone)]
pub struct PostHogConfig {
    #[primary_key]
    pub singleton: bool,
    pub host: String,
    pub project_api_key: String,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = posthog_admin_identity)]
pub struct PostHogAdminIdentity {
    #[primary_key]
    pub identity: Identity,
    pub added_at: Timestamp,
}

#[spacetimedb::table(
    accessor = posthog_outbox,
    index(accessor = by_status_created_at, btree(columns = [status, created_at])),
    index(accessor = by_status_next_attempt_at, btree(columns = [status, next_attempt_at])),
    index(accessor = by_status_claim_expires_at, btree(columns = [status, claim_expires_at])),
    index(accessor = by_status_updated_at, btree(columns = [status, updated_at]))
)]
#[derive(Clone, Debug)]
pub struct PostHogOutbox {
    #[primary_key]
    pub outbox_id: String,
    pub uuid: String,
    pub idempotency_key: Option<String>,
    pub distinct_id: String,
    pub event: String,
    pub properties_json: Option<String>,
    #[index(btree)]
    pub status: PostHogOutboxStatus,
    pub attempts: u32,
    pub claim_id: Option<String>,
    pub claim_expires_at: Timestamp,
    pub next_attempt_at: Timestamp,
    pub last_status_code: Option<u16>,
    pub last_error: Option<String>,
    #[index(btree)]
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub delivered_at: Option<Timestamp>,
}

/// Runs `posthog_scheduled_flush` every 5 seconds (created by [`install`]).
#[spacetimedb::table(accessor = posthog_flush_tick, scheduled(posthog_scheduled_flush))]
pub struct PostHogFlushTick {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

/// Outbox row counts: `pending` is Queued + Processing, `failed` is Failed + Rejected.
#[spacetimedb::table(accessor = posthog_delivery_stats)]
#[derive(Clone)]
pub struct PostHogDeliveryStats {
    #[primary_key]
    pub singleton: bool,
    pub pending: u64,
    pub delivered: u64,
    pub failed: u64,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = posthog_delivery_log)]
#[derive(Clone, Debug)]
pub struct PostHogDeliveryLog {
    #[primary_key]
    #[auto_inc]
    pub delivery_id: u64,
    pub source: PostHogDeliverySource,
    pub outbox_id: Option<String>,
    pub distinct_id: String,
    pub event: String,
    #[index(btree)]
    pub ok: bool,
    pub status_code: u16,
    pub response_body: String,
    pub error_message: Option<String>,
    #[index(btree)]
    pub attempted_at: Timestamp,
    /// `-attempted_at` in micros, so the index reads newest first.
    #[index(btree)]
    pub attempted_at_order: i64,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct PostHogDeliveryLogRow {
    pub delivery_id: u64,
    pub source: PostHogDeliverySource,
    pub outbox_id: Option<String>,
    pub distinct_id: String,
    pub event: String,
    pub ok: bool,
    pub status_code: u16,
    pub response_body: String,
    pub error_message: Option<String>,
    pub attempted_at: Timestamp,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct PostHogConfigStatus {
    pub is_configured: bool,
    pub host: Option<String>,
    pub project_api_key_length: u32,
}

#[derive(SpacetimeType, Clone, Debug, Default)]
pub struct PostHogFlushResult {
    pub attempted: u32,
    pub delivered: u32,
    pub failed: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct PostHogCaptureResult {
    pub ok: bool,
    pub status_code: u16,
    pub error: Option<String>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct PostHogFeatureFlagResult {
    pub ok: bool,
    pub status_code: u16,
    pub enabled: Option<bool>,
    pub variant: Option<String>,
    pub error: Option<String>,
}

/// Arguments for [`enqueue_event_in_tx`].
#[derive(Clone, Debug, Default)]
pub struct EnqueueEventArgs {
    pub distinct_id: String,
    pub event: String,
    pub properties_json: Option<String>,
    pub idempotency_key: Option<String>,
}

/// What [`enqueue_event_in_tx`] did: `error` is the rejection code for invalid input.
#[derive(Clone, Debug)]
pub struct EnqueueEventResult {
    pub outbox_id: String,
    pub inserted: bool,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Keeps the first 2,048 characters plus `...`.
pub fn truncate_for_log(body: &str) -> String {
    if js_len(body) <= MAX_LOG_BODY_LENGTH {
        body.to_string()
    } else {
        format!("{}...", js_slice(body, MAX_LOG_BODY_LENGTH))
    }
}

pub fn is_ok_status(status: u16) -> bool {
    (200..300).contains(&status)
}

/// 4xx responses other than 408 and 429 won't succeed on retry.
pub fn is_permanent_failure(status: u16) -> bool {
    (400..500).contains(&status) && status != 408 && status != 429
}

/// 1 s doubling per attempt, capped at 5 minutes.
pub fn retry_delay_micros(attempt: u32) -> i64 {
    let exponent = (attempt as i64 - 1).clamp(0, 30);
    (INITIAL_RETRY_DELAY_MICROS << exponent).min(MAX_RETRY_DELAY_MICROS)
}

/// `https://` or `http://` plus a host; trailing slashes dropped.
pub fn normalize_host(host: &str) -> Result<String, String> {
    let trimmed = host.trim();
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    match rest {
        Some(r) if !r.is_empty() && !r.starts_with('/') => {
            Ok(trimmed.trim_end_matches('/').to_string())
        }
        _ => Err(errors::INVALID_HOST.into()),
    }
}

/// A JSON object (not an array or null), else `None`.
pub fn parse_json_object(json: &str) -> Option<Value> {
    serde_json::from_str::<Value>(json)
        .ok()
        .filter(Value::is_object)
}

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

/// `JSON.stringify` as JS writes it: integer-like keys first, `1.0` as `1`.
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

/// `Timestamp.toISOString()` from the TS SDK: `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
pub fn iso_string(t: Timestamp) -> String {
    let micros = t.to_micros_since_unix_epoch();
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:06}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

fn ts_plus(t: Timestamp, micros: i64) -> Timestamp {
    Timestamp::from_micros_since_unix_epoch(t.to_micros_since_unix_epoch() + micros)
}

fn epoch() -> Timestamp {
    Timestamp::from_micros_since_unix_epoch(0)
}

/// `status` → which stats counter it belongs to.
fn stats_key(s: PostHogOutboxStatus) -> usize {
    match s {
        O::Delivered => 1,
        O::Failed | O::Rejected => 2,
        _ => 0,
    }
}

#[derive(Default)]
struct StatsDelta([i64; 3]);

impl StatsDelta {
    fn change(&mut self, from: Option<PostHogOutboxStatus>, to: Option<PostHogOutboxStatus>) {
        if let Some(f) = from {
            self.0[stats_key(f)] -= 1;
        }
        if let Some(t) = to {
            self.0[stats_key(t)] += 1;
        }
    }
    fn apply(&self, ctx: &ReducerContext) {
        if self.0 == [0, 0, 0] {
            return;
        }
        let existing = ctx.db.posthog_delivery_stats().singleton().find(true);
        let current = existing.clone().unwrap_or(PostHogDeliveryStats {
            singleton: true,
            pending: 0,
            delivered: 0,
            failed: 0,
            updated_at: ctx.timestamp,
        });
        let adjust = |v: u64, d: i64| (v as i64 + d).max(0) as u64;
        let row = PostHogDeliveryStats {
            singleton: true,
            pending: adjust(current.pending, self.0[0]),
            delivered: adjust(current.delivered, self.0[1]),
            failed: adjust(current.failed, self.0[2]),
            updated_at: ctx.timestamp,
        };
        if existing.is_some() {
            ctx.db.posthog_delivery_stats().singleton().update(row);
        } else {
            ctx.db.posthog_delivery_stats().insert(row);
        }
    }
}

// ---------------------------------------------------------------------------
// Admins and config
// ---------------------------------------------------------------------------

/// Call from your `init` reducer: seeds the publishing identity as admin and starts the
/// 5-second scheduled flush.
pub fn install(ctx: &ReducerContext) {
    if ctx
        .db
        .posthog_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        ctx.db
            .posthog_admin_identity()
            .insert(PostHogAdminIdentity {
                identity: ctx.sender(),
                added_at: ctx.timestamp,
            });
    }
    if ctx.db.posthog_flush_tick().count() == 0 {
        ctx.db.posthog_flush_tick().insert(PostHogFlushTick {
            scheduled_id: 0,
            scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(FLUSH_INTERVAL_MICROS)),
        });
    }
}

pub fn is_admin(ctx: &ReducerContext, who: Identity) -> bool {
    ctx.db
        .posthog_admin_identity()
        .identity()
        .find(who)
        .is_some()
}

fn require_admin(ctx: &ReducerContext, who: Identity) -> Result<(), String> {
    if is_admin(ctx, who) {
        Ok(())
    } else {
        Err(errors::NOT_AUTHORIZED.into())
    }
}

fn require_procedure_admin(ctx: &mut ProcedureContext) -> Result<(), String> {
    let sender = ctx.sender();
    ctx.with_tx(|tx| require_admin(tx, sender))
}

#[spacetimedb::procedure]
pub fn posthog_add_admin_identity(
    ctx: &mut ProcedureContext,
    identity: Identity,
) -> Result<(), String> {
    let sender = ctx.sender();
    let now = ctx.timestamp;
    ctx.try_with_tx(|tx| {
        require_admin(tx, sender)?;
        if tx
            .db
            .posthog_admin_identity()
            .identity()
            .find(identity)
            .is_none()
        {
            tx.db.posthog_admin_identity().insert(PostHogAdminIdentity {
                identity,
                added_at: now,
            });
        }
        Ok(())
    })
}

#[spacetimedb::procedure]
pub fn posthog_remove_admin_identity(
    ctx: &mut ProcedureContext,
    identity: Identity,
) -> Result<(), String> {
    let sender = ctx.sender();
    ctx.try_with_tx(|tx| {
        require_admin(tx, sender)?;
        if tx
            .db
            .posthog_admin_identity()
            .identity()
            .find(identity)
            .is_none()
        {
            return Ok(());
        }
        if tx.db.posthog_admin_identity().count() <= 1 {
            return Err(errors::CANNOT_REMOVE_LAST_ADMIN.to_string());
        }
        tx.db.posthog_admin_identity().identity().delete(identity);
        Ok(())
    })
}

/// Admins only. `host` must start with `https://` or `http://`, e.g. `https://us.i.posthog.com`.
#[spacetimedb::procedure]
pub fn set_posthog_config(
    ctx: &mut ProcedureContext,
    host: String,
    project_api_key: String,
) -> Result<(), String> {
    let host = normalize_host(&host)?;
    let key = project_api_key.trim().to_string();
    if key.is_empty() {
        return Err(errors::INVALID_PROJECT_API_KEY.into());
    }
    let sender = ctx.sender();
    let now = ctx.timestamp;
    ctx.try_with_tx(|tx| {
        require_admin(tx, sender)?;
        let row = PostHogConfig {
            singleton: true,
            host: host.clone(),
            project_api_key: key.clone(),
            updated_at: now,
        };
        if tx.db.posthog_config().singleton().find(true).is_some() {
            tx.db.posthog_config().singleton().update(row);
        } else {
            tx.db.posthog_config().insert(row);
        }
        Ok(())
    })
}

#[spacetimedb::procedure]
pub fn get_posthog_config_status(
    ctx: &mut ProcedureContext,
) -> Result<PostHogConfigStatus, String> {
    let sender = ctx.sender();
    ctx.try_with_tx(|tx| {
        require_admin(tx, sender)?;
        let row = tx.db.posthog_config().singleton().find(true);
        Ok(PostHogConfigStatus {
            is_configured: row.is_some(),
            host: row.as_ref().map(|r| r.host.clone()),
            project_api_key_length: row.map_or(0, |r| js_len(&r.project_api_key) as u32),
        })
    })
}

fn load_config(ctx: &mut ProcedureContext) -> Result<PostHogConfig, String> {
    ctx.with_tx(|tx| tx.db.posthog_config().singleton().find(true))
        .ok_or_else(|| errors::CONFIG_MISSING.to_string())
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// A PostHog response. Network failures are status 0 with the error as the body.
#[derive(Clone, Debug)]
pub struct PostHogHttpResult {
    pub ok: bool,
    pub status_code: u16,
    pub response_body: String,
}

fn posthog_fetch(
    ctx: &mut ProcedureContext,
    cfg: &PostHogConfig,
    path: &str,
    body: &Value,
) -> PostHogHttpResult {
    let request = spacetimedb::http::Request::builder()
        .method("POST")
        .uri(format!("{}{path}", cfg.host))
        .header("content-type", "application/json")
        .body(js_json(body));
    let response = match request
        .map_err(|e| e.to_string())
        .and_then(|r| ctx.http.send(r).map_err(|e| e.to_string()))
    {
        Ok(r) => r,
        Err(e) => {
            return PostHogHttpResult {
                ok: false,
                status_code: 0,
                response_body: e,
            };
        }
    };
    let status_code = response.status().as_u16();
    PostHogHttpResult {
        ok: is_ok_status(status_code),
        status_code,
        response_body: response.into_body().into_string_lossy(),
    }
}

/// A flag's value from a `/flags?v=2` body: its variant string, or its `enabled` boolean.
pub fn feature_flag_value(body: &str, key: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let flag = parsed.get("flags")?.as_object()?.get(key)?.as_object()?;
    let enabled = flag.get("enabled")?.as_bool()?;
    Some(match flag.get("variant") {
        Some(Value::String(v)) => Value::String(v.clone()),
        _ => Value::Bool(enabled),
    })
}

/// One event as sent in a `/batch` request.
struct BatchEvent<'a> {
    uuid: &'a str,
    created_at: Timestamp,
    distinct_id: &'a str,
    event: &'a str,
    properties_json: Option<&'a str>,
}

fn batch_body(project_api_key: &str, events: &[BatchEvent]) -> Value {
    let batch: Vec<Value> = events
        .iter()
        .map(|e| {
            let mut m = Map::new();
            m.insert("uuid".into(), Value::String(e.uuid.to_string()));
            m.insert("timestamp".into(), Value::String(iso_string(e.created_at)));
            m.insert(
                "distinct_id".into(),
                Value::String(e.distinct_id.to_string()),
            );
            m.insert("event".into(), Value::String(e.event.to_string()));
            m.insert(
                "properties".into(),
                e.properties_json
                    .and_then(parse_json_object)
                    .unwrap_or_else(|| Value::Object(Map::new())),
            );
            Value::Object(m)
        })
        .collect();
    let mut m = Map::new();
    m.insert("api_key".into(), Value::String(project_api_key.to_string()));
    m.insert("batch".into(), Value::Array(batch));
    Value::Object(m)
}

fn log_delivery(
    ctx: &ReducerContext,
    source: PostHogDeliverySource,
    outbox_id: Option<String>,
    distinct_id: &str,
    event: &str,
    result: &PostHogHttpResult,
) {
    let body = truncate_for_log(&result.response_body);
    ctx.db.posthog_delivery_log().insert(PostHogDeliveryLog {
        delivery_id: 0,
        source,
        outbox_id,
        distinct_id: distinct_id.to_string(),
        event: event.to_string(),
        ok: result.ok,
        status_code: result.status_code,
        error_message: (!result.ok).then(|| body.clone()),
        response_body: body,
        attempted_at: ctx.timestamp,
        attempted_at_order: -ctx.timestamp.to_micros_since_unix_epoch(),
    });
}

// ---------------------------------------------------------------------------
// The outbox
// ---------------------------------------------------------------------------

fn event_input_error(
    distinct_id: &str,
    event: &str,
    properties_json: Option<&str>,
) -> Option<&'static str> {
    let d = distinct_id.trim();
    let e = event.trim();
    if d.is_empty() || js_len(d) > MAX_DISTINCT_ID_LENGTH {
        return Some(errors::INVALID_DISTINCT_ID);
    }
    if e.is_empty() || js_len(e) > MAX_EVENT_NAME_LENGTH {
        return Some(errors::INVALID_EVENT);
    }
    if let Some(p) = properties_json {
        if js_len(p) > MAX_PROPERTIES_JSON_LENGTH {
            return Some(errors::PROPERTIES_TOO_LARGE);
        }
        if parse_json_object(p).is_none() {
            return Some(errors::INVALID_PROPERTIES_JSON);
        }
    }
    None
}

/// Queue an event in the caller's transaction. Never fails, so analytics can't roll back your
/// reducer: invalid input is stored as `Rejected` with the code in `last_error` (and returned as
/// `error`). An idempotency key that's already queued is a no-op.
pub fn enqueue_event_in_tx(ctx: &ReducerContext, args: EnqueueEventArgs) -> EnqueueEventResult {
    let key = args
        .idempotency_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);
    let error = event_input_error(
        &args.distinct_id,
        &args.event,
        args.properties_json.as_deref(),
    )
    .or_else(|| {
        key.as_ref()
            .filter(|k| js_len(k) > MAX_IDEMPOTENCY_KEY_LENGTH)
            .map(|_| errors::IDEMPOTENCY_KEY_TOO_LONG)
    })
    .map(str::to_string);
    let uuid = ctx.new_uuid_v7().map(|u| u.to_string()).unwrap_or_default();
    let outbox_id = match (&key, &error) {
        (Some(k), None) => format!("idem:{k}"),
        _ => format!("evt:{uuid}"),
    };
    if ctx
        .db
        .posthog_outbox()
        .outbox_id()
        .find(&outbox_id)
        .is_some()
    {
        return EnqueueEventResult {
            outbox_id,
            inserted: false,
            error: None,
        };
    }
    let status = if error.is_some() {
        O::Rejected
    } else {
        O::Queued
    };
    ctx.db.posthog_outbox().insert(PostHogOutbox {
        outbox_id: outbox_id.clone(),
        uuid,
        idempotency_key: if error.is_some() { None } else { key },
        distinct_id: js_slice(args.distinct_id.trim(), MAX_DISTINCT_ID_LENGTH),
        event: js_slice(args.event.trim(), MAX_EVENT_NAME_LENGTH),
        properties_json: if error.is_some() {
            None
        } else {
            args.properties_json
        },
        status,
        attempts: 0,
        claim_id: None,
        claim_expires_at: epoch(),
        next_attempt_at: ctx.timestamp,
        last_status_code: None,
        last_error: error.clone(),
        created_at: ctx.timestamp,
        updated_at: ctx.timestamp,
        delivered_at: None,
    });
    let mut delta = StatsDelta::default();
    delta.change(None, Some(status));
    delta.apply(ctx);
    EnqueueEventResult {
        outbox_id,
        inserted: true,
        error,
    }
}

/// Delete Delivered / Failed / Rejected rows and delivery log rows last updated 30+ days before
/// `now`: up to 1,000 per call. (The scheduled flush runs this every tick.)
pub fn prune_delivery_history(ctx: &ReducerContext, now: Timestamp) {
    let cutoff = ts_plus(now, -HISTORY_RETENTION_MICROS);
    let mut delta = StatsDelta::default();
    let mut budget = MAX_RETENTION_ROWS_PER_TICK;
    for status in [O::Delivered, O::Failed, O::Rejected] {
        let rows: Vec<PostHogOutbox> = ctx
            .db
            .posthog_outbox()
            .by_status_updated_at()
            .filter((status, ..=cutoff))
            .take(budget)
            .collect();
        budget -= rows.len();
        for row in rows {
            ctx.db.posthog_outbox().outbox_id().delete(&row.outbox_id);
            delta.change(Some(row.status), None);
        }
    }
    let logs: Vec<u64> = ctx
        .db
        .posthog_delivery_log()
        .attempted_at()
        .filter(..=cutoff)
        .take(budget)
        .map(|r| r.delivery_id)
        .collect();
    for id in logs {
        ctx.db.posthog_delivery_log().delivery_id().delete(id);
    }
    delta.apply(ctx);
}

/// Release expired claims, then claim up to `limit` due Queued rows.
fn claim_queued_rows(ctx: &ReducerContext, limit: usize, claim_id: &str) -> Vec<PostHogOutbox> {
    let now = ctx.timestamp;
    let expired: Vec<PostHogOutbox> = ctx
        .db
        .posthog_outbox()
        .by_status_claim_expires_at()
        .filter((O::Processing, ..=now))
        .take(MAX_EXPIRED_CLAIMS_PER_FLUSH)
        .collect();
    for mut row in expired {
        if row.claim_expires_at.to_micros_since_unix_epoch() > now.to_micros_since_unix_epoch() {
            continue;
        }
        row.status = O::Queued;
        row.claim_id = None;
        row.claim_expires_at = epoch();
        row.next_attempt_at = now;
        row.updated_at = now;
        ctx.db.posthog_outbox().outbox_id().update(row);
    }
    let due: Vec<PostHogOutbox> = ctx
        .db
        .posthog_outbox()
        .by_status_next_attempt_at()
        .filter((O::Queued, ..=now))
        .take(limit)
        .collect();
    let expires_at = ts_plus(now, CLAIM_TTL_MICROS);
    due.into_iter()
        .map(|mut row| {
            row.status = O::Processing;
            row.claim_id = Some(claim_id.to_string());
            row.claim_expires_at = expires_at;
            row.updated_at = now;
            ctx.db.posthog_outbox().outbox_id().update(row.clone());
            row
        })
        .collect()
}

fn deliver_outbox(
    ctx: &mut ProcedureContext,
    cfg: &PostHogConfig,
    limit: usize,
) -> PostHogFlushResult {
    let claim_id = ctx.new_uuid_v7().map(|u| u.to_string()).unwrap_or_default();
    let rows = ctx.with_tx(|tx| claim_queued_rows(tx, limit, &claim_id));
    if rows.is_empty() {
        return PostHogFlushResult::default();
    }
    let events: Vec<BatchEvent> = rows
        .iter()
        .map(|r| BatchEvent {
            uuid: &r.uuid,
            created_at: r.created_at,
            distinct_id: &r.distinct_id,
            event: &r.event,
            properties_json: r.properties_json.as_deref(),
        })
        .collect();
    let result = posthog_fetch(
        ctx,
        cfg,
        "/batch",
        &batch_body(&cfg.project_api_key, &events),
    );
    let now = ctx.timestamp;
    ctx.with_tx(|tx| {
        let (mut delivered, mut failed) = (0, 0);
        let mut delta = StatsDelta::default();
        for row in &rows {
            let Some(current) = tx.db.posthog_outbox().outbox_id().find(&row.outbox_id) else {
                continue;
            };
            if current.status != O::Processing
                || current.claim_id.as_deref() != Some(claim_id.as_str())
            {
                continue;
            }
            log_delivery(
                tx,
                PostHogDeliverySource::Flush,
                Some(row.outbox_id.clone()),
                &row.distinct_id,
                &row.event,
                &result,
            );
            let attempts = current.attempts + 1;
            let terminal = result.ok
                || is_permanent_failure(result.status_code)
                || attempts >= MAX_DELIVERY_ATTEMPTS;
            let status = if result.ok {
                O::Delivered
            } else if terminal {
                O::Failed
            } else {
                O::Queued
            };
            let settled = PostHogOutbox {
                status,
                attempts,
                claim_id: None,
                claim_expires_at: epoch(),
                next_attempt_at: if terminal {
                    now
                } else {
                    ts_plus(now, retry_delay_micros(current.attempts + 1))
                },
                last_status_code: Some(result.status_code),
                last_error: (!result.ok).then(|| truncate_for_log(&result.response_body)),
                updated_at: now,
                delivered_at: result.ok.then_some(now),
                ..current.clone()
            };
            delta.change(Some(current.status), Some(status));
            tx.db.posthog_outbox().outbox_id().update(settled);
            if result.ok {
                delivered += 1;
            } else {
                failed += 1;
            }
        }
        delta.apply(tx);
        PostHogFlushResult {
            attempted: rows.len() as u32,
            delivered,
            failed,
        }
    })
}

/// Runs every 5 s (see [`install`]): prunes history, then sends up to 10 batches of 100 while
/// full batches keep succeeding. Only the scheduler may call it.
#[spacetimedb::procedure]
pub fn posthog_scheduled_flush(ctx: &mut ProcedureContext, _tick: PostHogFlushTick) {
    if ctx.sender() != ctx.database_identity() {
        panic!("{}", errors::NOT_AUTHORIZED);
    }
    let cfg = ctx.with_tx(|tx| {
        prune_delivery_history(tx, tx.timestamp);
        tx.db.posthog_config().singleton().find(true)
    });
    let Some(cfg) = cfg else { return };
    for _ in 0..MAX_FLUSH_BATCHES_PER_TICK {
        if deliver_outbox(ctx, &cfg, MAX_FLUSH_LIMIT as usize).delivered < MAX_FLUSH_LIMIT {
            break;
        }
    }
}

/// Admins only: queue an event, failing on invalid input.
#[spacetimedb::reducer]
pub fn posthog_enqueue_event(
    ctx: &ReducerContext,
    distinct_id: String,
    event: String,
    properties_json: Option<String>,
    idempotency_key: Option<String>,
) -> Result<(), String> {
    require_admin(ctx, ctx.sender())?;
    let r = enqueue_event_in_tx(
        ctx,
        EnqueueEventArgs {
            distinct_id,
            event,
            properties_json,
            idempotency_key,
        },
    );
    match r.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Admins only: send one event now.
#[spacetimedb::procedure]
pub fn posthog_capture_now(
    ctx: &mut ProcedureContext,
    distinct_id: String,
    event: String,
    properties_json: Option<String>,
) -> Result<PostHogCaptureResult, String> {
    require_procedure_admin(ctx)?;
    if let Some(e) = event_input_error(&distinct_id, &event, properties_json.as_deref()) {
        return Err(e.into());
    }
    let cfg = load_config(ctx)?;
    let uuid = ctx.new_uuid_v7().map(|u| u.to_string()).unwrap_or_default();
    let ev = BatchEvent {
        uuid: &uuid,
        created_at: ctx.timestamp,
        distinct_id: &distinct_id,
        event: &event,
        properties_json: properties_json.as_deref(),
    };
    let result = posthog_fetch(
        ctx,
        &cfg,
        "/batch",
        &batch_body(&cfg.project_api_key, &[ev]),
    );
    ctx.with_tx(|tx| {
        log_delivery(
            tx,
            PostHogDeliverySource::Direct,
            None,
            &distinct_id,
            &event,
            &result,
        )
    });
    Ok(PostHogCaptureResult {
        ok: result.ok,
        status_code: result.status_code,
        error: (!result.ok).then(|| truncate_for_log(&result.response_body)),
    })
}

/// Admins only: send up to `limit` (1-100) queued events now.
#[spacetimedb::procedure]
pub fn posthog_flush_outbox(
    ctx: &mut ProcedureContext,
    limit: u32,
) -> Result<PostHogFlushResult, String> {
    require_procedure_admin(ctx)?;
    if limit == 0 || limit > MAX_FLUSH_LIMIT {
        return Err(errors::INVALID_FLUSH_LIMIT.into());
    }
    let cfg = load_config(ctx)?;
    Ok(deliver_outbox(ctx, &cfg, limit as usize))
}

/// Admins only: move up to `limit` Failed events back to Queued with a fresh retry budget.
#[spacetimedb::reducer]
pub fn posthog_requeue_failed_events(ctx: &ReducerContext, limit: u32) -> Result<(), String> {
    require_admin(ctx, ctx.sender())?;
    if limit == 0 || limit > MAX_ADMIN_BATCH {
        return Err(errors::INVALID_REQUEUE_LIMIT.into());
    }
    let mut delta = StatsDelta::default();
    let rows: Vec<PostHogOutbox> = ctx
        .db
        .posthog_outbox()
        .status()
        .filter(O::Failed)
        .take(limit as usize)
        .collect();
    for mut row in rows {
        delta.change(Some(row.status), Some(O::Queued));
        row.status = O::Queued;
        row.attempts = 0;
        row.claim_id = None;
        row.claim_expires_at = epoch();
        row.next_attempt_at = ctx.timestamp;
        row.updated_at = ctx.timestamp;
        ctx.db.posthog_outbox().outbox_id().update(row);
    }
    delta.apply(ctx);
    Ok(())
}

/// Admins only: delete up to `max_rows` outbox and delivery log rows (PostHog keeps what it got).
#[spacetimedb::reducer]
pub fn posthog_clear_analytics(ctx: &ReducerContext, max_rows: u32) -> Result<(), String> {
    require_admin(ctx, ctx.sender())?;
    if max_rows == 0 || max_rows > MAX_ADMIN_BATCH {
        return Err(errors::INVALID_CLEAR_BATCH.into());
    }
    let mut delta = StatsDelta::default();
    let outbox: Vec<PostHogOutbox> = ctx
        .db
        .posthog_outbox()
        .iter()
        .take(max_rows as usize)
        .collect();
    let left = max_rows as usize - outbox.len();
    for row in outbox {
        delta.change(Some(row.status), None);
        ctx.db.posthog_outbox().outbox_id().delete(&row.outbox_id);
    }
    let logs: Vec<u64> = ctx
        .db
        .posthog_delivery_log()
        .iter()
        .take(left)
        .map(|r| r.delivery_id)
        .collect();
    for id in logs {
        ctx.db.posthog_delivery_log().delivery_id().delete(id);
    }
    delta.apply(ctx);
    Ok(())
}

fn optional_json_object(
    json: Option<&str>,
    too_large: &str,
    invalid: &str,
) -> Result<Option<Value>, String> {
    let Some(j) = json else { return Ok(None) };
    if js_len(j) > MAX_PROPERTIES_JSON_LENGTH {
        return Err(too_large.into());
    }
    parse_json_object(j)
        .map(Some)
        .ok_or_else(|| invalid.to_string())
}

/// Admins only: evaluate a flag with PostHog `/flags?v=2`. `enabled` / `variant` are unset when
/// the flag isn't in the response.
#[spacetimedb::procedure]
pub fn posthog_get_feature_flag(
    ctx: &mut ProcedureContext,
    key: String,
    distinct_id: String,
    person_properties_json: Option<String>,
    groups_json: Option<String>,
) -> Result<PostHogFeatureFlagResult, String> {
    require_procedure_admin(ctx)?;
    if key.trim().is_empty() || js_len(&key) > MAX_EVENT_NAME_LENGTH {
        return Err(errors::INVALID_FLAG_KEY.into());
    }
    if distinct_id.trim().is_empty() || js_len(&distinct_id) > MAX_DISTINCT_ID_LENGTH {
        return Err(errors::INVALID_DISTINCT_ID.into());
    }
    let person = optional_json_object(
        person_properties_json.as_deref(),
        errors::PERSON_PROPERTIES_TOO_LARGE,
        errors::INVALID_PERSON_PROPERTIES_JSON,
    )?;
    let groups = optional_json_object(
        groups_json.as_deref(),
        errors::GROUPS_TOO_LARGE,
        errors::INVALID_GROUPS_JSON,
    )?;
    let cfg = load_config(ctx)?;
    let mut body = Map::new();
    body.insert("api_key".into(), Value::String(cfg.project_api_key.clone()));
    body.insert("distinct_id".into(), Value::String(distinct_id.clone()));
    if let Some(p) = person {
        body.insert("person_properties".into(), p);
    }
    if let Some(g) = groups {
        body.insert("groups".into(), g);
    }
    let result = posthog_fetch(ctx, &cfg, "/flags?v=2", &Value::Object(body));
    let value = if result.ok {
        feature_flag_value(&result.response_body, &key)
    } else {
        None
    };
    let flag_event = format!("$feature_flag:{key}");
    ctx.with_tx(|tx| {
        log_delivery(
            tx,
            PostHogDeliverySource::FeatureFlag,
            None,
            &distinct_id,
            &flag_event,
            &result,
        )
    });
    Ok(PostHogFeatureFlagResult {
        ok: result.ok,
        status_code: result.status_code,
        enabled: value.as_ref().map(|v| *v != Value::Bool(false)),
        variant: value.as_ref().and_then(Value::as_str).map(str::to_string),
        error: (!result.ok).then(|| truncate_for_log(&result.response_body)),
    })
}

// ---------------------------------------------------------------------------
// Admin views
// ---------------------------------------------------------------------------

/// Up to 500 in-flight and queued events (admins only; empty for everyone else).
#[spacetimedb::view(accessor = posthog_outbox_admin, public)]
fn posthog_outbox_admin(ctx: &ViewContext) -> Vec<PostHogOutbox> {
    if ctx
        .db
        .posthog_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        return Vec::new();
    }
    let mut rows: Vec<PostHogOutbox> = ctx
        .db
        .posthog_outbox()
        .status()
        .filter(O::Processing)
        .take(500)
        .collect();
    if rows.len() < 500 {
        let more = 500 - rows.len();
        rows.extend(
            ctx.db
                .posthog_outbox()
                .status()
                .filter(O::Queued)
                .take(more),
        );
    }
    rows
}

/// The 50 most recent delivery attempts (admins only).
#[spacetimedb::view(accessor = posthog_delivery_log_admin, public)]
fn posthog_delivery_log_admin(ctx: &ViewContext) -> Vec<PostHogDeliveryLogRow> {
    if ctx
        .db
        .posthog_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        return Vec::new();
    }
    ctx.db
        .posthog_delivery_log()
        .attempted_at_order()
        .filter(i64::MIN..)
        .take(50)
        .map(|r| PostHogDeliveryLogRow {
            delivery_id: r.delivery_id,
            source: r.source,
            outbox_id: r.outbox_id,
            distinct_id: r.distinct_id,
            event: r.event,
            ok: r.ok,
            status_code: r.status_code,
            response_body: r.response_body,
            error_message: r.error_message,
            attempted_at: r.attempted_at,
        })
        .collect()
}
