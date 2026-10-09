//! # api_keys.rs: API keys for server-to-server callers, for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/api-keys` (SpacetimeDB PR #5908, commit
//! `f6ed7dcd1c8f853c97fc7e9f126606fca7adadac`, package `spacetime-api-keys-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Same tables (`api_key`, `api_key_admin_identity`, `api_key_config`, `api_key_sweep_tick`,
//! `api_key_usage`), procedures (`create_api_key`, `create_api_key_for_subject`,
//! `rotate_api_key`), reducers (`revoke_api_key`, `revoke_api_key_for_subject`,
//! `set_api_keys_config`, `add_api_keys_admin`, `remove_api_keys_admin`, the scheduled
//! `api_keys_sweep`), views (`my_api_keys`, `api_keys_admin`, `api_key_usage_admin`), key
//! format, key derivation, hashing, scope rules and error codes as the TS package. Given the
//! same operator secret, counter and timestamp, every language derives the same key.
//!
//! Needs the crypto port next to it: copy `crypto/rust/crypto.rs` to `src/crypto.rs` and this
//! file to `src/api_keys.rs`, add `pub mod crypto; pub mod api_keys;` to `lib.rs`, and add
//! `sha2 = "0.10"` and `hmac = "0.12"` to `Cargo.toml`.
//!
//! ```ignore
//! #[spacetimedb::reducer(init)]
//! pub fn init(ctx: &ReducerContext) { api_keys::install(ctx); }
//!
//! // Then, once: spacetime call <db> set_api_keys_config '[{"some": "<32+ char secret>"}, {"none": []}]'
//!
//! // In a procedure or HTTP handler that receives a key:
//! let r = ctx.try_with_tx(|tx| Ok::<_, String>(api_keys::verify_api_key(tx, &api_keys::VerifyApiKeyArgs {
//!     key: key.clone(), required_scope: Some("files:write".into()), action: None,
//! })?))?;
//! if !r.allowed { return Err(r.reason); }
//! ```
//!
//! Changes from the TypeScript original:
//! - Procedures return `Result<T, String>` (TS procedures throw a `SenderError`).
//! - `api_keys_sweep` rejects callers other than the scheduler.
//! - JSON is parsed and re-serialized by a small built-in implementation that follows
//!   `JSON.parse`/`JSON.stringify` (key order with integer-like keys first, number formatting,
//!   string escaping), so normalized `scopes_json` and `metadata_json` match TS byte for byte.
//!   Nesting deeper than 512 levels is rejected (TS has no limit).
//! - [`run_usage_sweep`] is public, so hosts (and tests) can run the retention sweep directly.

use crate::crypto;
use spacetimedb::{
    Identity, ProcedureContext, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration,
    Timestamp, ViewContext,
};

const DEFAULT_KEY_PREFIX: &str = "stdb_live";
const MAX_NAME_LENGTH: usize = 120;
const MAX_OWNER_SUBJECT_LENGTH: usize = 256;
const MAX_SCOPE_LENGTH: usize = 128;
const MAX_SCOPES: usize = 128;
const MAX_METADATA_JSON_LENGTH: usize = 8192;
const MAX_RAW_KEY_LENGTH: usize = 128;
const MAX_ACTIVE_KEYS_PER_OWNER: usize = 50;
const MAX_EXPIRATION_SECONDS: u32 = 60 * 60 * 24 * 365 * 10;
const MIN_SECRET_LENGTH: usize = 32;
const MIN_USAGE_RETENTION_SECONDS: u32 = 60 * 60;
pub const DEFAULT_USAGE_RETENTION_SECONDS: u32 = 60 * 60 * 24 * 30;
const USAGE_SWEEP_BATCH: usize = 1000;
const ONE_SECOND_MICROS: i64 = 1_000_000;
const SWEEP_INTERVAL_MICROS: i64 = 60 * ONE_SECOND_MICROS;
/// Hex characters of the secret stored as the lookup prefix (64 bits).
const LOOKUP_SECRET_CHARS: usize = 16;

/// The `api_keys.*` codes.
pub mod errors {
    pub const NOT_AUTHORIZED: &str = "api_keys.not_authorized";
    pub const CANNOT_REMOVE_LAST_ADMIN: &str = "api_keys.cannot_remove_last_admin";
    pub const CONFIG_MISSING: &str = "api_keys.config_missing";
    pub const SECRET_REQUIRED: &str = "api_keys.secret_required";
    pub const INVALID_SECRET: &str = "api_keys.invalid_secret";
    pub const INVALID_USAGE_RETENTION: &str = "api_keys.invalid_usage_retention_seconds";
    pub const INVALID_KEY_PREFIX: &str = "api_keys.invalid_key_prefix";
    pub const INVALID_NAME: &str = "api_keys.invalid_name";
    pub const INVALID_OWNER_SUBJECT: &str = "api_keys.invalid_owner_subject";
    pub const INVALID_ACTION: &str = "api_keys.invalid_action";
    pub const INVALID_REQUIRED_SCOPE: &str = "api_keys.invalid_required_scope";
    pub const INVALID_SCOPES_JSON: &str = "api_keys.invalid_scopes_json";
    pub const INVALID_METADATA_JSON: &str = "api_keys.invalid_metadata_json";
    pub const INVALID_EXPIRES_IN_SECONDS: &str = "api_keys.invalid_expires_in_seconds";
    pub const ACTIVE_KEY_LIMIT_REACHED: &str = "api_keys.active_key_limit_reached";
    pub const INVALID_KEY_ID: &str = "api_keys.invalid_key_id";
    pub const NOT_FOUND: &str = "api_keys.not_found";
    pub const KEY_REVOKED: &str = "api_keys.key_revoked";
    pub const KEY_EXPIRED: &str = "api_keys.key_expired";
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiKeyStatus {
    Active,
    Revoked,
}

#[spacetimedb::table(accessor = api_key)]
#[derive(Clone, Debug)]
pub struct ApiKey {
    #[primary_key]
    pub key_id: String,
    #[unique]
    pub prefix: String,
    pub hash: String,
    #[index(btree)]
    pub owner_subject: String,
    pub name: String,
    pub scopes_json: String,
    pub metadata_json: Option<String>,
    #[index(btree)]
    pub status: ApiKeyStatus,
    #[index(btree)]
    pub created_at: Timestamp,
    #[index(btree)]
    pub created_at_order: i64,
    pub expires_at: Option<Timestamp>,
    pub last_used_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
}

#[spacetimedb::table(accessor = api_key_admin_identity)]
pub struct ApiKeyAdminIdentity {
    #[primary_key]
    pub identity: Identity,
    pub added_at: Timestamp,
}

/// Created by `set_api_keys_config`. `counter` advances with every derived secret.
#[spacetimedb::table(accessor = api_key_config)]
pub struct ApiKeyConfig {
    #[primary_key]
    pub singleton: bool,
    pub secret: String,
    pub counter: u64,
    pub usage_retention_seconds: u32,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = api_key_sweep_tick, scheduled(api_keys_sweep))]
pub struct ApiKeySweepTick {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

#[spacetimedb::table(accessor = api_key_usage)]
#[derive(Clone, Debug)]
pub struct ApiKeyUsage {
    #[primary_key]
    #[auto_inc]
    pub usage_id: u64,
    #[index(btree)]
    pub key_id: String,
    pub prefix: String,
    #[index(btree)]
    pub owner_subject: String,
    #[index(btree)]
    pub action: String,
    #[index(btree)]
    pub allowed: bool,
    #[index(btree)]
    pub reason: String,
    #[index(btree)]
    pub used_at: Timestamp,
    #[index(btree)]
    pub used_at_order: i64,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ApiKeySummary {
    pub key_id: String,
    pub prefix: String,
    pub owner_subject: String,
    pub name: String,
    pub scopes_json: String,
    pub metadata_json: Option<String>,
    pub status: ApiKeyStatus,
    pub created_at: Timestamp,
    pub expires_at: Option<Timestamp>,
    pub last_used_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
}

/// The raw `key` is only ever returned here; the database stores its SHA-256.
#[derive(SpacetimeType, Clone, Debug)]
pub struct ApiKeyCreateResult {
    pub key_id: String,
    pub key: String,
    pub prefix: String,
    pub owner_subject: String,
    pub name: String,
    pub scopes_json: String,
    pub metadata_json: Option<String>,
    pub status: ApiKeyStatus,
    pub created_at: Timestamp,
    pub expires_at: Option<Timestamp>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ApiKeyUsageSummary {
    pub usage_id: u64,
    pub key_id: String,
    pub prefix: String,
    pub owner_subject: String,
    pub action: String,
    pub allowed: bool,
    pub reason: String,
    pub used_at: Timestamp,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ApiKeyVerifyResult {
    pub allowed: bool,
    pub reason: String,
    pub key_id: Option<String>,
    pub prefix: Option<String>,
    pub owner_subject: Option<String>,
    pub scopes_json: Option<String>,
    pub metadata_json: Option<String>,
}

/// Arguments for [`create_api_key_in_tx`].
#[derive(Clone, Debug, Default)]
pub struct CreateApiKeyArgs {
    pub owner_subject: String,
    pub name: String,
    pub scopes_json: String,
    pub metadata_json: Option<String>,
    pub expires_in_seconds: Option<u32>,
    pub key_prefix: Option<String>,
}

/// Arguments for [`verify_api_key`].
#[derive(Clone, Debug, Default)]
pub struct VerifyApiKeyArgs {
    pub key: String,
    pub required_scope: Option<String>,
    pub action: Option<String>,
}

// ---------------------------------------------------------------------------
// Keys (src/keys.ts)
// ---------------------------------------------------------------------------

/// `ctx.rng()` is seeded from the call timestamp, so it can't produce secrets. Key material is
/// HMAC-SHA256 keyed by the operator's secret over a database-wide counter and the transaction
/// timestamp. The counter makes each draw unique; the timestamp separates databases recreated
/// with one secret.
pub fn derive_key_secret(operator_secret: &str, counter: u64, micros: i64) -> [u8; 32] {
    crypto::hmac_sha256(
        operator_secret.as_bytes(),
        format!("spacetimedb-api-keys secret {counter} {micros}").as_bytes(),
    )
}

/// Formats a key as `{key_prefix}_{hex secret}`, with its stored lookup prefix.
pub fn format_api_key(key_prefix: &str, secret: &[u8]) -> (String, String) {
    let hex = crypto::bytes_to_hex(secret);
    (
        format!("{key_prefix}_{hex}"),
        format!("{key_prefix}_{}", &hex[..LOOKUP_SECRET_CHARS]),
    )
}

/// The lookup prefix of a raw key, or `None` when it can't be one of ours.
pub fn extract_lookup_prefix(key: &str) -> Option<String> {
    let trimmed: Vec<u16> = js_trim(key).encode_utf16().collect();
    let last = trimmed.iter().rposition(|&c| c == b'_' as u16)?;
    if last == 0 {
        return None;
    }
    let secret = &trimmed[last + 1..];
    if secret.len() < LOOKUP_SECRET_CHARS {
        return None;
    }
    let mut out = trimmed[..last + 1].to_vec();
    out.extend_from_slice(&secret[..LOOKUP_SECRET_CHARS]);
    Some(String::from_utf16_lossy(&out))
}

pub fn hash_api_key(key: &str) -> String {
    crypto::bytes_to_hex(&crypto::sha256(key.as_bytes()))
}

pub fn matches_api_key_hash(key: &str, expected_hex: &str) -> bool {
    match crypto::hex_to_bytes(expected_hex) {
        Ok(expected) => crypto::timing_safe_equal(&expected, &crypto::sha256(key.as_bytes())),
        Err(_) => false,
    }
}

/// Whether `scopes_json` (a JSON array of scope strings) grants `required_scope`. `*` grants
/// everything and `files:*` grants every `files:` scope. A missing or blank requirement passes.
pub fn has_scope(scopes_json: &str, required_scope: Option<&str>) -> bool {
    let Some(required) = required_scope.map(js_trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let Some(Json::Array(scopes)) = json::parse(scopes_json) else {
        return false;
    };
    let required: Vec<u16> = required.encode_utf16().collect();
    scopes.iter().any(|s| match s {
        Json::String(scope) => {
            scope == &[b'*' as u16]
                || *scope == required
                || (scope.ends_with(&[b':' as u16, b'*' as u16])
                    && required.starts_with(&scope[..scope.len() - 1]))
        }
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// Operations (src/submodule/operations.ts)
// ---------------------------------------------------------------------------

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn is_scope_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '*' | '.' | '-')
}

/// `/^[A-Za-z][A-Za-z0-9_]{1,31}$/`
fn normalize_key_prefix(prefix: Option<&str>) -> Result<String, String> {
    let value = js_trim(prefix.unwrap_or(DEFAULT_KEY_PREFIX));
    let mut chars = value.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && (2..=32).contains(&value.len())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(errors::INVALID_KEY_PREFIX.into());
    }
    Ok(value.to_string())
}

/// Trimmed, with runs of whitespace collapsed to one space.
fn normalize_name(name: &str) -> Result<String, String> {
    let value = name
        .split(is_js_whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let len = utf16_len(&value);
    if len == 0 || len > MAX_NAME_LENGTH {
        return Err(errors::INVALID_NAME.into());
    }
    Ok(value)
}

/// JavaScript's `\s` (also what `String.prototype.trim` removes).
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

fn normalize_owner_subject(owner_subject: &str) -> Result<String, String> {
    let value = js_trim(owner_subject);
    let len = utf16_len(value);
    if len == 0 || len > MAX_OWNER_SUBJECT_LENGTH {
        return Err(errors::INVALID_OWNER_SUBJECT.into());
    }
    Ok(value.to_string())
}

fn normalize_action(action: Option<&str>, required_scope: Option<&str>) -> Result<String, String> {
    let value = js_trim(action.or(required_scope).unwrap_or("verify"));
    let len = utf16_len(value);
    if len == 0 || len > MAX_SCOPE_LENGTH {
        return Err(errors::INVALID_ACTION.into());
    }
    Ok(value.to_string())
}

fn normalize_required_scope(required_scope: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = required_scope else {
        return Ok(None);
    };
    let value = js_trim(raw);
    if value.is_empty() || utf16_len(value) > MAX_SCOPE_LENGTH || !value.chars().all(is_scope_char)
    {
        return Err(errors::INVALID_REQUIRED_SCOPE.into());
    }
    Ok(Some(value.to_string()))
}

fn normalize_scopes_json(scopes_json: &str) -> Result<String, String> {
    let err = || errors::INVALID_SCOPES_JSON.to_string();
    let Json::Array(items) = json::parse(scopes_json).ok_or_else(err)? else {
        return Err(err());
    };
    if items.is_empty() || items.len() > MAX_SCOPES {
        return Err(err());
    }
    let mut scopes: Vec<String> = Vec::new();
    for item in items {
        let Json::String(raw) = item else {
            return Err(err());
        };
        let raw = String::from_utf16(&raw).map_err(|_| err())?;
        let scope = js_trim(&raw);
        if scope.is_empty()
            || utf16_len(scope) > MAX_SCOPE_LENGTH
            || !scope.chars().all(is_scope_char)
        {
            return Err(err());
        }
        if !scopes.iter().any(|s| s == scope) {
            scopes.push(scope.to_string());
        }
    }
    Ok(json::stringify(&Json::Array(
        scopes
            .iter()
            .map(|s| Json::String(s.encode_utf16().collect()))
            .collect(),
    )))
}

fn normalize_metadata_json(metadata_json: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = metadata_json else {
        return Ok(None);
    };
    let value = js_trim(raw);
    if value.is_empty() {
        return Ok(None);
    }
    if utf16_len(value) > MAX_METADATA_JSON_LENGTH {
        return Err(errors::INVALID_METADATA_JSON.into());
    }
    match json::parse(value) {
        Some(parsed @ Json::Object(_)) => Ok(Some(json::stringify(&parsed))),
        _ => Err(errors::INVALID_METADATA_JSON.into()),
    }
}

fn expires_at_from_seconds(
    ctx: &ReducerContext,
    expires_in_seconds: Option<u32>,
) -> Result<Option<Timestamp>, String> {
    let Some(seconds) = expires_in_seconds else {
        return Ok(None);
    };
    if seconds == 0 || seconds > MAX_EXPIRATION_SECONDS {
        return Err(errors::INVALID_EXPIRES_IN_SECONDS.into());
    }
    Ok(Some(
        ctx.timestamp + TimeDuration::from_micros(seconds as i64 * ONE_SECOND_MICROS),
    ))
}

fn is_expired(row: &ApiKey, now: Timestamp) -> bool {
    row.expires_at.is_some_and(|e| e <= now)
}

/// The TS `toHexString()` of an identity: lowercase hex, no `0x`.
pub fn identity_hex(identity: Identity) -> String {
    identity.to_hex().to_string()
}

fn to_summary(row: ApiKey) -> ApiKeySummary {
    ApiKeySummary {
        key_id: row.key_id,
        prefix: row.prefix,
        owner_subject: row.owner_subject,
        name: row.name,
        scopes_json: row.scopes_json,
        metadata_json: row.metadata_json,
        status: row.status,
        created_at: row.created_at,
        expires_at: row.expires_at,
        last_used_at: row.last_used_at,
        revoked_at: row.revoked_at,
    }
}

fn to_create_result(row: &ApiKey, key: String) -> ApiKeyCreateResult {
    ApiKeyCreateResult {
        key_id: row.key_id.clone(),
        key,
        prefix: row.prefix.clone(),
        owner_subject: row.owner_subject.clone(),
        name: row.name.clone(),
        scopes_json: row.scopes_json.clone(),
        metadata_json: row.metadata_json.clone(),
        status: row.status,
        created_at: row.created_at,
        expires_at: row.expires_at,
    }
}

fn generate_raw_key(ctx: &ReducerContext, key_prefix: &str) -> Result<(String, String), String> {
    let cfg = ctx
        .db
        .api_key_config()
        .singleton()
        .find(true)
        .ok_or(errors::CONFIG_MISSING)?;
    let counter = cfg.counter + 1;
    let secret = derive_key_secret(
        &cfg.secret,
        counter,
        ctx.timestamp.to_micros_since_unix_epoch(),
    );
    ctx.db
        .api_key_config()
        .singleton()
        .update(ApiKeyConfig { counter, ..cfg });
    Ok(format_api_key(key_prefix, &secret))
}

struct Usage<'a> {
    key_id: &'a str,
    prefix: &'a str,
    owner_subject: &'a str,
    action: &'a str,
    allowed: bool,
    reason: &'a str,
}

fn record_usage(ctx: &ReducerContext, u: Usage) {
    ctx.db.api_key_usage().insert(ApiKeyUsage {
        usage_id: 0,
        key_id: u.key_id.into(),
        prefix: u.prefix.into(),
        owner_subject: u.owner_subject.into(),
        action: u.action.into(),
        allowed: u.allowed,
        reason: u.reason.into(),
        used_at: ctx.timestamp,
        used_at_order: -ctx.timestamp.to_micros_since_unix_epoch(),
    });
}

pub fn is_admin(ctx: &ReducerContext, identity: Identity) -> bool {
    ctx.db
        .api_key_admin_identity()
        .identity()
        .find(identity)
        .is_some()
}

/// `Err(api_keys.not_authorized)` unless the caller is an API key admin.
pub fn require_admin(ctx: &ReducerContext) -> Result<(), String> {
    if is_admin(ctx, ctx.sender()) {
        Ok(())
    } else {
        Err(errors::NOT_AUTHORIZED.into())
    }
}

/// Create a key in the caller's transaction. At most 50 active keys per owner.
pub fn create_api_key_in_tx(
    ctx: &ReducerContext,
    args: &CreateApiKeyArgs,
) -> Result<ApiKeyCreateResult, String> {
    let owner_subject = normalize_owner_subject(&args.owner_subject)?;
    let name = normalize_name(&args.name)?;
    let scopes_json = normalize_scopes_json(&args.scopes_json)?;
    let metadata_json = normalize_metadata_json(args.metadata_json.as_deref())?;
    let expires_at = expires_at_from_seconds(ctx, args.expires_in_seconds)?;
    let key_prefix = normalize_key_prefix(args.key_prefix.as_deref())?;
    let active = ctx
        .db
        .api_key()
        .owner_subject()
        .filter(&owner_subject)
        .filter(|r| r.status == ApiKeyStatus::Active && !is_expired(r, ctx.timestamp))
        .count();
    if active >= MAX_ACTIVE_KEYS_PER_OWNER {
        return Err(errors::ACTIVE_KEY_LIMIT_REACHED.into());
    }
    let (key, prefix) = generate_raw_key(ctx, &key_prefix)?;
    let uuid = ctx.new_uuid_v7().map_err(|e| e.to_string())?;
    let row = ctx.db.api_key().insert(ApiKey {
        key_id: format!("ak_{uuid}"),
        prefix,
        hash: hash_api_key(&key),
        owner_subject,
        name,
        scopes_json,
        metadata_json,
        status: ApiKeyStatus::Active,
        created_at: ctx.timestamp,
        created_at_order: -ctx.timestamp.to_micros_since_unix_epoch(),
        expires_at,
        last_used_at: None,
        revoked_at: None,
    });
    record_usage(
        ctx,
        Usage {
            key_id: &row.key_id,
            prefix: &row.prefix,
            owner_subject: &row.owner_subject,
            action: "create",
            allowed: true,
            reason: "created",
        },
    );
    Ok(to_create_result(&row, key))
}

fn denied(prefix: Option<String>, reason: &str) -> ApiKeyVerifyResult {
    ApiKeyVerifyResult {
        allowed: false,
        reason: reason.into(),
        key_id: None,
        prefix,
        owner_subject: None,
        scopes_json: None,
        metadata_json: None,
    }
}

/// Check a raw key (and optionally a scope) in the caller's transaction. Allowed and denied
/// checks against a known key are recorded in `api_key_usage`; malformed and unknown keys
/// aren't. Errors only for an invalid `required_scope` or `action`.
pub fn verify_api_key(
    ctx: &ReducerContext,
    args: &VerifyApiKeyArgs,
) -> Result<ApiKeyVerifyResult, String> {
    let required_scope = normalize_required_scope(args.required_scope.as_deref())?;
    let action = normalize_action(args.action.as_deref(), required_scope.as_deref())?;
    let key = js_trim(&args.key);
    let len = utf16_len(key);
    if len == 0 || len > MAX_RAW_KEY_LENGTH {
        return Ok(denied(None, "invalid_key"));
    }
    let Some(prefix) = extract_lookup_prefix(key) else {
        return Ok(denied(None, "invalid_key"));
    };
    let Some(row) = ctx.db.api_key().prefix().find(&prefix) else {
        return Ok(denied(Some(prefix), "unknown_key"));
    };
    let deny = |reason: &str| -> Result<ApiKeyVerifyResult, String> {
        record_usage(
            ctx,
            Usage {
                key_id: &row.key_id,
                prefix: &row.prefix,
                owner_subject: &row.owner_subject,
                action: &action,
                allowed: false,
                reason,
            },
        );
        Ok(denied(Some(row.prefix.clone()), reason))
    };
    if !matches_api_key_hash(key, &row.hash) {
        return deny("invalid_key");
    }
    if row.status != ApiKeyStatus::Active {
        return deny("revoked");
    }
    if is_expired(&row, ctx.timestamp) {
        return deny("expired");
    }
    if !has_scope(&row.scopes_json, required_scope.as_deref()) {
        return deny("scope_denied");
    }
    ctx.db.api_key().key_id().update(ApiKey {
        last_used_at: Some(ctx.timestamp),
        ..row.clone()
    });
    record_usage(
        ctx,
        Usage {
            key_id: &row.key_id,
            prefix: &row.prefix,
            owner_subject: &row.owner_subject,
            action: &action,
            allowed: true,
            reason: "allowed",
        },
    );
    Ok(ApiKeyVerifyResult {
        allowed: true,
        reason: "allowed".into(),
        key_id: Some(row.key_id),
        prefix: Some(row.prefix),
        owner_subject: Some(row.owner_subject),
        scopes_json: Some(row.scopes_json),
        metadata_json: row.metadata_json,
    })
}

fn find_managed_key(
    ctx: &ReducerContext,
    key_id: &str,
    owner_subject: Option<&str>,
) -> Result<ApiKey, String> {
    let key_id = js_trim(key_id);
    if key_id.is_empty() {
        return Err(errors::INVALID_KEY_ID.into());
    }
    let row = ctx
        .db
        .api_key()
        .key_id()
        .find(key_id.to_string())
        .ok_or(errors::NOT_FOUND)?;
    let subject = normalize_owner_subject(
        &owner_subject.map_or_else(|| identity_hex(ctx.sender()), str::to_string),
    )?;
    if row.owner_subject != subject && !is_admin(ctx, ctx.sender()) {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    Ok(row)
}

/// Revoke a key. Owners (by subject) and admins may. Revoking twice is a no-op.
pub fn revoke_api_key_in_tx(
    ctx: &ReducerContext,
    key_id: &str,
    owner_subject: Option<&str>,
) -> Result<(), String> {
    let row = find_managed_key(ctx, key_id, owner_subject)?;
    if row.status == ApiKeyStatus::Revoked {
        return Ok(());
    }
    let row = ctx.db.api_key().key_id().update(ApiKey {
        status: ApiKeyStatus::Revoked,
        revoked_at: Some(ctx.timestamp),
        ..row
    });
    record_usage(
        ctx,
        Usage {
            key_id: &row.key_id,
            prefix: &row.prefix,
            owner_subject: &row.owner_subject,
            action: "revoke",
            allowed: true,
            reason: "revoked",
        },
    );
    Ok(())
}

/// Replace a live key's secret (same key id). Keeps the expiry unless a new one is given.
pub fn rotate_api_key_in_tx(
    ctx: &ReducerContext,
    key_id: &str,
    owner_subject: Option<&str>,
    expires_in_seconds: Option<u32>,
    key_prefix: Option<&str>,
) -> Result<ApiKeyCreateResult, String> {
    let row = find_managed_key(ctx, key_id, owner_subject)?;
    // Only live keys rotate, so rotation never revives a key or exceeds the cap.
    if row.status != ApiKeyStatus::Active {
        return Err(errors::KEY_REVOKED.into());
    }
    if is_expired(&row, ctx.timestamp) {
        return Err(errors::KEY_EXPIRED.into());
    }
    let key_prefix = normalize_key_prefix(key_prefix)?;
    let expires_at = match expires_in_seconds {
        None => row.expires_at,
        Some(_) => expires_at_from_seconds(ctx, expires_in_seconds)?,
    };
    let (key, prefix) = generate_raw_key(ctx, &key_prefix)?;
    let updated = ctx.db.api_key().key_id().update(ApiKey {
        prefix: prefix.clone(),
        hash: hash_api_key(&key),
        expires_at,
        last_used_at: None,
        ..row
    });
    record_usage(
        ctx,
        Usage {
            key_id: &updated.key_id,
            prefix: &prefix,
            owner_subject: &updated.owner_subject,
            action: "rotate",
            allowed: true,
            reason: "rotated",
        },
    );
    Ok(to_create_result(&updated, key))
}

/// Delete up to 1,000 usage rows older than the retention window. Returns the count.
pub fn run_usage_sweep(ctx: &ReducerContext) -> u32 {
    let retention = ctx
        .db
        .api_key_config()
        .singleton()
        .find(true)
        .map_or(DEFAULT_USAGE_RETENTION_SECONDS, |c| {
            c.usage_retention_seconds
        });
    let cutoff = Timestamp::from_micros_since_unix_epoch(
        ctx.timestamp.to_micros_since_unix_epoch() - retention as i64 * ONE_SECOND_MICROS,
    );
    let expired: Vec<u64> = ctx
        .db
        .api_key_usage()
        .used_at()
        .filter(Timestamp::from_micros_since_unix_epoch(i64::MIN)..=cutoff)
        .take(USAGE_SWEEP_BATCH)
        .map(|r| r.usage_id)
        .collect();
    for id in &expired {
        ctx.db.api_key_usage().usage_id().delete(id);
    }
    expired.len() as u32
}

// ---------------------------------------------------------------------------
// Install, client operations
// ---------------------------------------------------------------------------

/// Call from your `init` reducer: the caller becomes an API key admin and the usage sweep
/// runs every 60 s. Set the operator secret afterwards with `set_api_keys_config`.
pub fn install(ctx: &ReducerContext) {
    if ctx
        .db
        .api_key_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        ctx.db.api_key_admin_identity().insert(ApiKeyAdminIdentity {
            identity: ctx.sender(),
            added_at: ctx.timestamp,
        });
    }
    if ctx.db.api_key_sweep_tick().count() == 0 {
        ctx.db.api_key_sweep_tick().insert(ApiKeySweepTick {
            scheduled_id: 0,
            scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(SWEEP_INTERVAL_MICROS)),
        });
    }
}

/// Create a key owned by the caller.
#[spacetimedb::procedure]
pub fn create_api_key(
    ctx: &mut ProcedureContext,
    name: String,
    scopes_json: String,
    metadata_json: Option<String>,
    expires_in_seconds: Option<u32>,
    key_prefix: Option<String>,
) -> Result<ApiKeyCreateResult, String> {
    let owner_subject = identity_hex(ctx.sender());
    ctx.try_with_tx(|tx| {
        create_api_key_in_tx(
            tx,
            &CreateApiKeyArgs {
                owner_subject: owner_subject.clone(),
                name: name.clone(),
                scopes_json: scopes_json.clone(),
                metadata_json: metadata_json.clone(),
                expires_in_seconds,
                key_prefix: key_prefix.clone(),
            },
        )
    })
}

/// Create a key for any subject (admins).
#[spacetimedb::procedure]
pub fn create_api_key_for_subject(
    ctx: &mut ProcedureContext,
    owner_subject: String,
    name: String,
    scopes_json: String,
    metadata_json: Option<String>,
    expires_in_seconds: Option<u32>,
    key_prefix: Option<String>,
) -> Result<ApiKeyCreateResult, String> {
    ctx.try_with_tx(|tx| {
        require_admin(tx)?;
        create_api_key_in_tx(
            tx,
            &CreateApiKeyArgs {
                owner_subject: owner_subject.clone(),
                name: name.clone(),
                scopes_json: scopes_json.clone(),
                metadata_json: metadata_json.clone(),
                expires_in_seconds,
                key_prefix: key_prefix.clone(),
            },
        )
    })
}

#[spacetimedb::procedure]
pub fn rotate_api_key(
    ctx: &mut ProcedureContext,
    key_id: String,
    expires_in_seconds: Option<u32>,
    key_prefix: Option<String>,
) -> Result<ApiKeyCreateResult, String> {
    let subject = identity_hex(ctx.sender());
    ctx.try_with_tx(|tx| {
        rotate_api_key_in_tx(
            tx,
            &key_id,
            Some(&subject),
            expires_in_seconds,
            key_prefix.as_deref(),
        )
    })
}

#[spacetimedb::reducer]
pub fn revoke_api_key(ctx: &ReducerContext, key_id: String) -> Result<(), String> {
    revoke_api_key_in_tx(ctx, &key_id, Some(&identity_hex(ctx.sender())))
}

#[spacetimedb::reducer]
pub fn revoke_api_key_for_subject(
    ctx: &ReducerContext,
    key_id: String,
    owner_subject: String,
) -> Result<(), String> {
    require_admin(ctx)?;
    revoke_api_key_in_tx(ctx, &key_id, Some(&owner_subject))
}

/// Admins. `secret` (32+ characters, generated outside the module) is required on the first
/// call; later calls may change either setting.
#[spacetimedb::reducer]
pub fn set_api_keys_config(
    ctx: &ReducerContext,
    secret: Option<String>,
    usage_retention_seconds: Option<u32>,
) -> Result<(), String> {
    require_admin(ctx)?;
    let existing = ctx.db.api_key_config().singleton().find(true);
    let secret = secret
        .or_else(|| existing.as_ref().map(|c| c.secret.clone()))
        .ok_or(errors::SECRET_REQUIRED)?;
    if utf16_len(&secret) < MIN_SECRET_LENGTH {
        return Err(errors::INVALID_SECRET.into());
    }
    let usage_retention_seconds = usage_retention_seconds
        .or(existing.as_ref().map(|c| c.usage_retention_seconds))
        .unwrap_or(DEFAULT_USAGE_RETENTION_SECONDS);
    if !(MIN_USAGE_RETENTION_SECONDS..=MAX_EXPIRATION_SECONDS).contains(&usage_retention_seconds) {
        return Err(errors::INVALID_USAGE_RETENTION.into());
    }
    let row = ApiKeyConfig {
        singleton: true,
        secret,
        counter: existing.as_ref().map_or(0, |c| c.counter),
        usage_retention_seconds,
        updated_at: ctx.timestamp,
    };
    if existing.is_some() {
        ctx.db.api_key_config().singleton().update(row);
    } else {
        ctx.db.api_key_config().insert(row);
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn add_api_keys_admin(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .api_key_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        ctx.db.api_key_admin_identity().insert(ApiKeyAdminIdentity {
            identity,
            added_at: ctx.timestamp,
        });
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn remove_api_keys_admin(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .api_key_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        return Ok(());
    }
    if ctx.db.api_key_admin_identity().count() <= 1 {
        return Err(errors::CANNOT_REMOVE_LAST_ADMIN.into());
    }
    ctx.db.api_key_admin_identity().identity().delete(identity);
    Ok(())
}

/// Deletes a bounded batch of usage rows older than the retention window. Every 60 s.
#[spacetimedb::reducer]
pub fn api_keys_sweep(ctx: &ReducerContext, _tick: ApiKeySweepTick) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    run_usage_sweep(ctx);
    Ok(())
}

/// Up to 500 of the caller's keys (never the raw key or its hash).
#[spacetimedb::view(accessor = my_api_keys, public)]
fn my_api_keys(ctx: &ViewContext) -> Vec<ApiKeySummary> {
    ctx.db
        .api_key()
        .owner_subject()
        .filter(&identity_hex(ctx.sender()))
        .take(500)
        .map(to_summary)
        .collect()
}

/// The 200 newest keys, for admins (others get nothing).
#[spacetimedb::view(accessor = api_keys_admin, public)]
fn api_keys_admin(ctx: &ViewContext) -> Vec<ApiKeySummary> {
    if ctx
        .db
        .api_key_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        return Vec::new();
    }
    ctx.db
        .api_key()
        .created_at_order()
        .filter(i64::MIN..)
        .take(200)
        .map(to_summary)
        .collect()
}

/// The 100 newest usage records, for admins (others get nothing).
#[spacetimedb::view(accessor = api_key_usage_admin, public)]
fn api_key_usage_admin(ctx: &ViewContext) -> Vec<ApiKeyUsageSummary> {
    if ctx
        .db
        .api_key_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        return Vec::new();
    }
    ctx.db
        .api_key_usage()
        .used_at_order()
        .filter(i64::MIN..)
        .take(100)
        .map(|r| ApiKeyUsageSummary {
            usage_id: r.usage_id,
            key_id: r.key_id,
            prefix: r.prefix,
            owner_subject: r.owner_subject,
            action: r.action,
            allowed: r.allowed,
            reason: r.reason,
            used_at: r.used_at,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// JSON: just enough of JSON.parse / JSON.stringify to normalize like TS
// ---------------------------------------------------------------------------

/// A parsed JSON value. Strings are UTF-16, as in JavaScript (so lone surrogates survive).
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(Vec<u16>),
    Array(Vec<Json>),
    /// In JavaScript property order: integer-like keys ascending, then insertion order.
    Object(Vec<(Vec<u16>, Json)>),
}

pub mod json {
    use super::Json;

    const MAX_DEPTH: usize = 512;

    /// `JSON.parse`: strict JSON, whitespace is space, tab, CR and LF only. `None` if invalid.
    pub fn parse(text: &str) -> Option<Json> {
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
        };
        p.ws();
        let v = p.value(0).ok()?;
        p.ws();
        (p.i == p.s.len()).then_some(v)
    }

    struct Parser<'a> {
        s: &'a [u8],
        i: usize,
    }

    impl Parser<'_> {
        fn ws(&mut self) {
            while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            }
        }
        fn eat(&mut self, b: u8) -> bool {
            if self.s.get(self.i) == Some(&b) {
                self.i += 1;
                true
            } else {
                false
            }
        }
        fn lit(&mut self, word: &str, v: Json) -> Result<Json, ()> {
            if self.s[self.i..].starts_with(word.as_bytes()) {
                self.i += word.len();
                Ok(v)
            } else {
                Err(())
            }
        }
        fn value(&mut self, depth: usize) -> Result<Json, ()> {
            if depth > MAX_DEPTH {
                return Err(());
            }
            match self.s.get(self.i).ok_or(())? {
                b'{' => {
                    self.i += 1;
                    let mut fields: Vec<(Vec<u16>, Json)> = Vec::new();
                    self.ws();
                    if !self.eat(b'}') {
                        loop {
                            self.ws();
                            if self.s.get(self.i) != Some(&b'"') {
                                return Err(());
                            }
                            let k = self.string()?;
                            self.ws();
                            if !self.eat(b':') {
                                return Err(());
                            }
                            self.ws();
                            let v = self.value(depth + 1)?;
                            match fields.iter_mut().find(|(key, _)| *key == k) {
                                Some(slot) => slot.1 = v,
                                None => fields.push((k, v)),
                            }
                            self.ws();
                            if self.eat(b',') {
                                continue;
                            }
                            if self.eat(b'}') {
                                break;
                            }
                            return Err(());
                        }
                    }
                    // Integer-like keys come first, ascending (a stable sort keeps the rest in order).
                    fields.sort_by_key(|(k, _)| array_index(k).map_or((1, 0), |n| (0, n)));
                    Ok(Json::Object(fields))
                }
                b'[' => {
                    self.i += 1;
                    let mut items = Vec::new();
                    self.ws();
                    if !self.eat(b']') {
                        loop {
                            self.ws();
                            items.push(self.value(depth + 1)?);
                            self.ws();
                            if self.eat(b',') {
                                continue;
                            }
                            if self.eat(b']') {
                                break;
                            }
                            return Err(());
                        }
                    }
                    Ok(Json::Array(items))
                }
                b'"' => Ok(Json::String(self.string()?)),
                b't' => self.lit("true", Json::Bool(true)),
                b'f' => self.lit("false", Json::Bool(false)),
                b'n' => self.lit("null", Json::Null),
                _ => self.number(),
            }
        }
        fn number(&mut self) -> Result<Json, ()> {
            let start = self.i;
            self.eat(b'-');
            let digits = |p: &mut Self| {
                let st = p.i;
                while p.i < p.s.len() && p.s[p.i].is_ascii_digit() {
                    p.i += 1;
                }
                p.i - st
            };
            if self.eat(b'0') {
                if self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
                    return Err(());
                }
            } else if digits(self) == 0 {
                return Err(());
            }
            if self.eat(b'.') && digits(self) == 0 {
                return Err(());
            }
            if self.eat(b'e') || self.eat(b'E') {
                if !self.eat(b'+') {
                    self.eat(b'-');
                }
                if digits(self) == 0 {
                    return Err(());
                }
            }
            let text = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| ())?;
            text.parse::<f64>().map(Json::Number).map_err(|_| ())
        }
        fn hex4(&mut self) -> Result<u16, ()> {
            let h = self.s.get(self.i..self.i + 4).ok_or(())?;
            let h = std::str::from_utf8(h).map_err(|_| ())?;
            if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(());
            }
            self.i += 4;
            u16::from_str_radix(h, 16).map_err(|_| ())
        }
        fn string(&mut self) -> Result<Vec<u16>, ()> {
            self.i += 1; // opening quote
            let mut out: Vec<u16> = Vec::new();
            loop {
                let b = *self.s.get(self.i).ok_or(())?;
                match b {
                    b'"' => {
                        self.i += 1;
                        return Ok(out);
                    }
                    b'\\' => {
                        self.i += 1;
                        let e = *self.s.get(self.i).ok_or(())?;
                        self.i += 1;
                        match e {
                            b'"' => out.push(0x22),
                            b'\\' => out.push(0x5c),
                            b'/' => out.push(0x2f),
                            b'b' => out.push(0x08),
                            b'f' => out.push(0x0c),
                            b'n' => out.push(0x0a),
                            b'r' => out.push(0x0d),
                            b't' => out.push(0x09),
                            b'u' => out.push(self.hex4()?),
                            _ => return Err(()),
                        }
                    }
                    0..=0x1f => return Err(()),
                    _ => {
                        // Copy one UTF-8 character as UTF-16.
                        let rest = std::str::from_utf8(&self.s[self.i..]).map_err(|_| ())?;
                        let c = rest.chars().next().ok_or(())?;
                        let mut buf = [0u16; 2];
                        out.extend_from_slice(c.encode_utf16(&mut buf));
                        self.i += c.len_utf8();
                    }
                }
            }
        }
    }

    /// A canonical array index ("0", "1", ... "4294967294"), as JavaScript orders them.
    fn array_index(k: &[u16]) -> Option<u64> {
        if k.is_empty() || k.len() > 10 || (k.len() > 1 && k[0] == b'0' as u16) {
            return None;
        }
        let mut n: u64 = 0;
        for &c in k {
            if !(b'0' as u16..=b'9' as u16).contains(&c) {
                return None;
            }
            n = n * 10 + (c - b'0' as u16) as u64;
        }
        (n < u32::MAX as u64).then_some(n)
    }

    /// `JSON.stringify` (compact).
    pub fn stringify(v: &Json) -> String {
        let mut out = String::new();
        write(v, &mut out);
        out
    }

    fn write(v: &Json, out: &mut String) {
        match v {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => out.push_str(&number(*n)),
            Json::String(s) => quote(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            Json::Object(fields) => {
                out.push('{');
                for (i, (k, val)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    quote(k, out);
                    out.push(':');
                    write(val, out);
                }
                out.push('}');
            }
        }
    }

    /// JSON's QuoteJSONString: short escapes, \u00xx for other controls and lone surrogates.
    fn quote(s: &[u16], out: &mut String) {
        out.push('"');
        for r in char::decode_utf16(s.iter().copied()) {
            match r {
                Ok('"') => out.push_str("\\\""),
                Ok('\\') => out.push_str("\\\\"),
                Ok('\u{8}') => out.push_str("\\b"),
                Ok('\u{c}') => out.push_str("\\f"),
                Ok('\n') => out.push_str("\\n"),
                Ok('\r') => out.push_str("\\r"),
                Ok('\t') => out.push_str("\\t"),
                Ok(c) if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                Ok(c) => out.push(c),
                Err(e) => out.push_str(&format!("\\u{:04x}", e.unpaired_surrogate())),
            }
        }
        out.push('"');
    }

    /// `Number.prototype.toString()` for finite numbers; `null` otherwise (as `JSON.stringify`).
    pub fn number(x: f64) -> String {
        if !x.is_finite() {
            return "null".into();
        }
        if x == 0.0 {
            return "0".into();
        }
        // Shortest round-trip digits and exponent, then ECMAScript's Number::toString layout.
        let e = format!("{:e}", x.abs());
        let (mantissa, exp) = e.split_once('e').unwrap();
        let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
        let k = digits.len() as i32;
        let n = exp.parse::<i32>().unwrap() + 1;
        let mut s = String::new();
        if x < 0.0 {
            s.push('-');
        }
        if k <= n && n <= 21 {
            s.push_str(&digits);
            s.push_str(&"0".repeat((n - k) as usize));
        } else if 0 < n && n <= 21 {
            s.push_str(&digits[..n as usize]);
            s.push('.');
            s.push_str(&digits[n as usize..]);
        } else if -6 < n && n <= 0 {
            s.push_str("0.");
            s.push_str(&"0".repeat((-n) as usize));
            s.push_str(&digits);
        } else {
            s.push_str(&digits[..1]);
            if k > 1 {
                s.push('.');
                s.push_str(&digits[1..]);
            }
            s.push('e');
            s.push(if n > 0 { '+' } else { '-' });
            s.push_str(&(n - 1).abs().to_string());
        }
        s
    }
}
