//! # presence.rs: who is here, with leases that expire, for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/presence` (SpacetimeDB PR #5908, commit
//! `f6ed7dcd1c8f853c97fc7e9f126606fca7adadac`, package `spacetime-presence-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Same tables (`presence_entry`, `presence_config`, `presence_admin_identity`,
//! `presence_sweep_tick`), keys, limits, error codes and views (`presence_online`,
//! `presence_entries_admin`) as the TS package.
//!
//! ```ignore
//! #[spacetimedb::reducer(init)]
//! pub fn init(ctx: &ReducerContext) { presence::install(ctx); }
//!
//! // Your own scopes, from your own reducers:
//! presence::upsert_presence(ctx, &presence::PresenceUpsertOpts {
//!     scope: format!("room:{room}"),
//!     subject: ctx.sender().to_hex().to_string(),
//!     status: Some("typing".into()),
//!     ..Default::default()
//! })?;
//! ```
//!
//! Changes from the TypeScript original:
//! - The generic names are prefixed: `heartbeat` → `presence_heartbeat`, `run_sweep` →
//!   `presence_run_sweep`, `update_config` → `presence_update_config` (TS mounts them under
//!   a submodule namespace; a drop-in shares the host's namespace).
//! - Procedures return `Result<T, String>` (TS procedures throw a `SenderError`).
//! - `presence_sweep` rejects callers other than the scheduler.
//! - `presence_entries_admin` walks the `expires_at` index: Rust views can't scan tables.

use spacetimedb::{
    Identity, ProcedureContext, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration,
    Timestamp, ViewContext,
};

const ONE_SECOND_MICROS: i64 = 1_000_000;
pub const DEFAULT_PRESENCE_TTL_SECONDS: u32 = 30;
pub const DEFAULT_PRESENCE_SWEEP_BATCH: u32 = 500;
pub const MAX_PRESENCE_SWEEP_BATCH: u32 = 10_000;
pub const DEFAULT_PRESENCE_STATUS: &str = "online";
const MAX_SCOPE_LENGTH: usize = 128;
const MAX_SUBJECT_LENGTH: usize = 256;
const MAX_STATUS_LENGTH: usize = 64;
const MAX_ACTIVITY_LENGTH: usize = 256;
const MAX_PAYLOAD_LENGTH: usize = 4096;
const MAX_TTL_SECONDS: u32 = 3600;
const MAX_VIEW_ROWS: usize = 1000;
const SWEEP_INTERVAL_MICROS: i64 = 10 * ONE_SECOND_MICROS;

/// The scope the client-callable operations write. Hosts choose their own scopes.
pub const GLOBAL_SCOPE: &str = "presence.global";

/// The `presence.*` codes.
pub mod errors {
    pub const NOT_AUTHORIZED: &str = "presence.not_authorized";
    pub const CANNOT_REMOVE_LAST_ADMIN: &str = "presence.cannot_remove_last_admin";
    pub const CONFIG_MISSING: &str = "presence.config_missing";
    pub const INVALID_SCOPE: &str = "presence.invalid_scope";
    pub const INVALID_SUBJECT: &str = "presence.invalid_subject";
    pub const INVALID_STATUS: &str = "presence.invalid_status";
    pub const INVALID_ACTIVITY: &str = "presence.invalid_activity";
    pub const INVALID_PAYLOAD: &str = "presence.invalid_payload";
    pub const INVALID_TTL_SECONDS: &str = "presence.invalid_ttl_seconds";
    pub const INVALID_SWEEP_BATCH: &str = "presence.invalid_sweep_batch";
}

#[spacetimedb::table(accessor = presence_entry)]
#[derive(Clone, Debug, PartialEq)]
pub struct PresenceEntry {
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub scope: String,
    #[index(btree)]
    pub subject: String,
    #[index(btree)]
    pub status: String,
    pub activity: Option<String>,
    pub payload_json: Option<String>,
    #[index(btree)]
    pub joined_at: Timestamp,
    #[index(btree)]
    pub last_seen_at: Timestamp,
    #[index(btree)]
    pub expires_at: Timestamp,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = presence_config)]
pub struct PresenceConfig {
    #[primary_key]
    pub singleton: bool,
    pub default_ttl_seconds: u32,
    pub sweep_batch: u32,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = presence_admin_identity)]
pub struct PresenceAdminIdentity {
    #[primary_key]
    pub identity: Identity,
    pub added_at: Timestamp,
}

#[spacetimedb::table(accessor = presence_sweep_tick, scheduled(presence_sweep))]
pub struct PresenceSweepTick {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

/// What `presence_heartbeat` returns.
#[derive(SpacetimeType, Clone, Debug)]
pub struct PresenceHeartbeatResult {
    pub scope: String,
    pub subject: String,
    pub status: String,
    pub expires_at: Timestamp,
}

/// Options for [`upsert_presence`]. `status` defaults to `"online"`, `ttl_seconds` to the
/// configured default.
#[derive(Clone, Debug, Default)]
pub struct PresenceUpsertOpts {
    pub scope: String,
    pub subject: String,
    pub status: Option<String>,
    pub activity: Option<String>,
    pub payload_json: Option<String>,
    pub ttl_seconds: Option<u32>,
}

/// Lengths are UTF-16 code units, as in TS, so keys and limits match across languages.
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn sanitize(code: &str, value: &str, max_length: usize) -> Result<String, String> {
    let out = value.trim();
    let len = utf16_len(out);
    if len == 0 || len > max_length {
        return Err(code.into());
    }
    Ok(out.to_string())
}

fn assert_ttl(ttl_seconds: u32) -> Result<(), String> {
    if ttl_seconds == 0 || ttl_seconds > MAX_TTL_SECONDS {
        return Err(errors::INVALID_TTL_SECONDS.into());
    }
    Ok(())
}

fn assert_sweep_batch(value: u32) -> Result<(), String> {
    if value == 0 || value > MAX_PRESENCE_SWEEP_BATCH {
        return Err(errors::INVALID_SWEEP_BATCH.into());
    }
    Ok(())
}

fn plus_seconds(ts: Timestamp, seconds: u32) -> Timestamp {
    ts + TimeDuration::from_micros(seconds as i64 * ONE_SECOND_MICROS)
}

fn default_ttl(ctx: &ReducerContext) -> u32 {
    ctx.db
        .presence_config()
        .singleton()
        .find(true)
        .map_or(DEFAULT_PRESENCE_TTL_SECONDS, |c| c.default_ttl_seconds)
}

/// The stored key: both parts trimmed and length-prefixed so delimiters can't collide.
pub fn build_presence_key(scope: &str, subject: &str) -> Result<String, String> {
    let scope = sanitize(errors::INVALID_SCOPE, scope, MAX_SCOPE_LENGTH)?;
    let subject = sanitize(errors::INVALID_SUBJECT, subject, MAX_SUBJECT_LENGTH)?;
    Ok(format!(
        "{}:{}{}:{}",
        utf16_len(&scope),
        scope,
        utf16_len(&subject),
        subject
    ))
}

/// Inserts the config row once. Later calls keep the stored config.
pub fn install_presence_config(
    ctx: &ReducerContext,
    default_ttl_seconds: u32,
    sweep_batch: u32,
) -> Result<(), String> {
    assert_ttl(default_ttl_seconds)?;
    assert_sweep_batch(sweep_batch)?;
    if ctx.db.presence_config().singleton().find(true).is_some() {
        return Ok(());
    }
    ctx.db.presence_config().insert(PresenceConfig {
        singleton: true,
        default_ttl_seconds,
        sweep_batch,
        updated_at: ctx.timestamp,
    });
    Ok(())
}

pub fn update_presence_config(
    ctx: &ReducerContext,
    default_ttl_seconds: u32,
    sweep_batch: u32,
) -> Result<(), String> {
    assert_ttl(default_ttl_seconds)?;
    assert_sweep_batch(sweep_batch)?;
    let existing = ctx
        .db
        .presence_config()
        .singleton()
        .find(true)
        .ok_or(errors::CONFIG_MISSING)?;
    ctx.db.presence_config().singleton().update(PresenceConfig {
        default_ttl_seconds,
        sweep_batch,
        updated_at: ctx.timestamp,
        ..existing
    });
    Ok(())
}

/// Insert or refresh a presence entry, in the caller's transaction.
pub fn upsert_presence(
    ctx: &ReducerContext,
    opts: &PresenceUpsertOpts,
) -> Result<PresenceEntry, String> {
    let scope = sanitize(errors::INVALID_SCOPE, &opts.scope, MAX_SCOPE_LENGTH)?;
    let subject = sanitize(errors::INVALID_SUBJECT, &opts.subject, MAX_SUBJECT_LENGTH)?;
    let key = build_presence_key(&scope, &subject)?;
    let ttl_seconds = opts.ttl_seconds.unwrap_or_else(|| default_ttl(ctx));
    assert_ttl(ttl_seconds)?;

    let status = sanitize(
        errors::INVALID_STATUS,
        opts.status.as_deref().unwrap_or(DEFAULT_PRESENCE_STATUS),
        MAX_STATUS_LENGTH,
    )?;
    let activity = trimmed_or_none(opts.activity.as_deref());
    if activity.as_deref().map_or(0, utf16_len) > MAX_ACTIVITY_LENGTH {
        return Err(errors::INVALID_ACTIVITY.into());
    }
    let payload_json = trimmed_or_none(opts.payload_json.as_deref());
    if payload_json.as_deref().map_or(0, utf16_len) > MAX_PAYLOAD_LENGTH {
        return Err(errors::INVALID_PAYLOAD.into());
    }

    let now = ctx.timestamp;
    let expires_at = plus_seconds(now, ttl_seconds);
    match ctx.db.presence_entry().key().find(&key) {
        None => Ok(ctx.db.presence_entry().insert(PresenceEntry {
            key,
            scope,
            subject,
            status,
            activity,
            payload_json,
            joined_at: now,
            last_seen_at: now,
            expires_at,
            updated_at: now,
        })),
        Some(existing) => Ok(ctx.db.presence_entry().key().update(PresenceEntry {
            status,
            activity,
            payload_json,
            last_seen_at: now,
            expires_at,
            updated_at: now,
            ..existing
        })),
    }
}

/// `opts.activity?.trim() || undefined`
fn trimmed_or_none(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Extends a lease and keeps its status, activity, and payload. Creates the entry (status
/// `"online"`) when there is none.
pub fn touch_presence(
    ctx: &ReducerContext,
    scope: &str,
    subject: &str,
    ttl_seconds: Option<u32>,
) -> Result<PresenceEntry, String> {
    let key = build_presence_key(scope, subject)?;
    let Some(existing) = ctx.db.presence_entry().key().find(&key) else {
        return upsert_presence(
            ctx,
            &PresenceUpsertOpts {
                scope: scope.into(),
                subject: subject.into(),
                ttl_seconds,
                ..Default::default()
            },
        );
    };
    let ttl = ttl_seconds.unwrap_or_else(|| default_ttl(ctx));
    assert_ttl(ttl)?;
    let now = ctx.timestamp;
    Ok(ctx.db.presence_entry().key().update(PresenceEntry {
        last_seen_at: now,
        expires_at: plus_seconds(now, ttl),
        updated_at: now,
        ..existing
    }))
}

/// Delete an entry. Returns whether one existed.
pub fn remove_presence(ctx: &ReducerContext, scope: &str, subject: &str) -> Result<bool, String> {
    let key = build_presence_key(scope, subject)?;
    Ok(ctx.db.presence_entry().key().delete(&key))
}

/// Deletes up to the configured sweep batch of rows that have expired. Returns the count.
pub fn run_presence_sweep(ctx: &ReducerContext) -> u32 {
    let batch = ctx
        .db
        .presence_config()
        .singleton()
        .find(true)
        .map_or(DEFAULT_PRESENCE_SWEEP_BATCH, |c| c.sweep_batch);
    let expired: Vec<String> = ctx
        .db
        .presence_entry()
        .expires_at()
        .filter(Timestamp::from_micros_since_unix_epoch(i64::MIN)..=ctx.timestamp)
        .take(batch as usize)
        .map(|r| r.key)
        .collect();
    for key in &expired {
        ctx.db.presence_entry().key().delete(key);
    }
    expired.len() as u32
}

// ---------------------------------------------------------------------------
// Install, admin, client operations
// ---------------------------------------------------------------------------

/// Call from your `init` reducer: the caller becomes a presence admin, config is seeded, and
/// expiry sweeps run every 10 s.
pub fn install(ctx: &ReducerContext) {
    if ctx
        .db
        .presence_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        ctx.db
            .presence_admin_identity()
            .insert(PresenceAdminIdentity {
                identity: ctx.sender(),
                added_at: ctx.timestamp,
            });
    }
    install_presence_config(
        ctx,
        DEFAULT_PRESENCE_TTL_SECONDS,
        DEFAULT_PRESENCE_SWEEP_BATCH,
    )
    .expect("default presence config is valid");
    if ctx.db.presence_sweep_tick().count() == 0 {
        ctx.db.presence_sweep_tick().insert(PresenceSweepTick {
            scheduled_id: 0,
            scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(SWEEP_INTERVAL_MICROS)),
        });
    }
}

pub fn is_admin(ctx: &ReducerContext, identity: Identity) -> bool {
    ctx.db
        .presence_admin_identity()
        .identity()
        .find(identity)
        .is_some()
}

/// `Err(presence.not_authorized)` unless the caller is a presence admin.
pub fn require_admin(ctx: &ReducerContext) -> Result<(), String> {
    if is_admin(ctx, ctx.sender()) {
        Ok(())
    } else {
        Err(errors::NOT_AUTHORIZED.into())
    }
}

/// Records the caller's global presence. The subject is the caller's identity (hex).
/// TS: `heartbeat`.
#[spacetimedb::procedure]
pub fn presence_heartbeat(
    ctx: &mut ProcedureContext,
    status: Option<String>,
    activity: Option<String>,
    payload_json: Option<String>,
    ttl_seconds: Option<u32>,
) -> Result<PresenceHeartbeatResult, String> {
    let subject = ctx.sender().to_hex().to_string();
    ctx.try_with_tx(|tx| {
        let row = upsert_presence(
            tx,
            &PresenceUpsertOpts {
                scope: GLOBAL_SCOPE.into(),
                subject: subject.clone(),
                status: status.clone(),
                activity: activity.clone(),
                payload_json: payload_json.clone(),
                ttl_seconds,
            },
        )?;
        Ok(PresenceHeartbeatResult {
            scope: row.scope,
            subject: row.subject,
            status: row.status,
            expires_at: row.expires_at,
        })
    })
}

#[spacetimedb::reducer]
pub fn clear_presence(ctx: &ReducerContext) -> Result<(), String> {
    remove_presence(ctx, GLOBAL_SCOPE, &ctx.sender().to_hex()).map(|_| ())
}

/// Sweep now (admins). Returns how many entries were deleted. TS: `run_sweep`.
#[spacetimedb::procedure]
pub fn presence_run_sweep(ctx: &mut ProcedureContext) -> Result<u32, String> {
    ctx.try_with_tx(|tx| {
        require_admin(tx)?;
        Ok(run_presence_sweep(tx))
    })
}

#[spacetimedb::reducer]
pub fn add_presence_admin(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .presence_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        ctx.db
            .presence_admin_identity()
            .insert(PresenceAdminIdentity {
                identity,
                added_at: ctx.timestamp,
            });
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn remove_presence_admin(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .presence_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        return Ok(());
    }
    if ctx.db.presence_admin_identity().count() <= 1 {
        return Err(errors::CANNOT_REMOVE_LAST_ADMIN.into());
    }
    ctx.db.presence_admin_identity().identity().delete(identity);
    Ok(())
}

/// Admins. TS: `update_config`.
#[spacetimedb::reducer]
pub fn presence_update_config(
    ctx: &ReducerContext,
    default_ttl_seconds: u32,
    sweep_batch: u32,
) -> Result<(), String> {
    require_admin(ctx)?;
    update_presence_config(ctx, default_ttl_seconds, sweep_batch)
}

/// Up to 1,000 global presence entries, including their activity and payload.
#[spacetimedb::view(accessor = presence_online, public)]
fn presence_online(ctx: &ViewContext) -> Vec<PresenceEntry> {
    ctx.db
        .presence_entry()
        .scope()
        .filter(GLOBAL_SCOPE)
        .take(MAX_VIEW_ROWS)
        .collect()
}

/// Up to 1,000 entries in every scope, for admins (others get nothing).
#[spacetimedb::view(accessor = presence_entries_admin, public)]
fn presence_entries_admin(ctx: &ViewContext) -> Vec<PresenceEntry> {
    if ctx
        .db
        .presence_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        return Vec::new();
    }
    // Views can't scan a table in 2.11, only read indexes: walk the whole expires_at index.
    ctx.db
        .presence_entry()
        .expires_at()
        .filter(Timestamp::from_micros_since_unix_epoch(i64::MIN)..)
        .take(MAX_VIEW_ROWS)
        .collect()
}

/// Scheduled every 10 s by [`install`].
#[spacetimedb::reducer]
pub fn presence_sweep(ctx: &ReducerContext, _tick: PresenceSweepTick) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    run_presence_sweep(ctx);
    Ok(())
}
