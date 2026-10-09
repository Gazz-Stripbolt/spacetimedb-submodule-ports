//! # rate_limit.rs: fixed-window rate limits for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/rate-limit` (SpacetimeDB PR #5903, commit
//! `ae25f1da24b1723e28acd0038e75e598f33952c3`, package `spacetime-rate-limit-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Same tables (`rate_limit_bucket`, `rate_limit_admin_identity`, `rate_limit_config`,
//! `rate_limit_sweep_tick`), bucket keys, error codes and window semantics as the TS package.
//!
//! ```ignore
//! const LOGIN: rate_limit::RateLimitPolicy = rate_limit::policy("auth.login", 5, 60);
//!
//! #[spacetimedb::reducer(init)]
//! pub fn init(ctx: &ReducerContext) { rate_limit::install(ctx); }
//!
//! #[spacetimedb::reducer]
//! pub fn login(ctx: &ReducerContext, user: String) -> Result<(), String> {
//!     let r = LOGIN.consume(ctx, &ctx.sender().to_hex(), 1)?;
//!     if !r.allowed { return Err(format!("slow down, try again in {}s", r.retry_after_seconds)); }
//!     // ...
//!     Ok(())
//! }
//! ```
//!
//! Changes from the TypeScript original:
//! - Policies are `const` values. TS rejects a second `client()` for the same scope while the
//!   module loads; Rust has no load-time hook, so a scope used with two different limits or
//!   windows fails on use with `rate_limit.duplicate_scope`, and policy validation errors
//!   (`rate_limit.invalid_scope/limit/window`) are also reported on use.
//! - The generic reducer names are prefixed: `update_config` → `rate_limit_update_config`,
//!   `reset_buckets` → `rate_limit_reset_buckets`, `run_sweep` → `rate_limit_run_sweep`
//!   (TS mounts them under a submodule namespace; a drop-in shares the host's namespace).
//! - `peek` works from reducers and views through the [`RateLimitRead`] trait.

use spacetimedb::{
    Identity, ProcedureContext, ReducerContext, ScheduleAt, Table, TimeDuration, Timestamp,
    ViewContext,
};
use std::cell::RefCell;
use std::collections::HashMap;

const ONE_SECOND_MICROS: i64 = 1_000_000;
pub const DEFAULT_SWEEP_BATCH: u32 = 500;
pub const MAX_SWEEP_BATCH: u32 = 10_000;
const MAX_SCOPE_LENGTH: usize = 128;
const MAX_ACTOR_KEY_LENGTH: usize = 256;
const SWEEP_INTERVAL_MICROS: i64 = 30_000_000;

/// The `rate_limit.*` codes.
pub mod errors {
    pub const NOT_AUTHORIZED: &str = "rate_limit.not_authorized";
    pub const CANNOT_REMOVE_LAST_ADMIN: &str = "rate_limit.cannot_remove_last_admin";
    pub const INVALID_ACTOR_KEY: &str = "rate_limit.invalid_actor_key";
    pub const INVALID_COST: &str = "rate_limit.invalid_cost";
    pub const INVALID_SWEEP_BATCH: &str = "rate_limit.invalid_sweep_batch";
    pub const INVALID_MAX_ROWS: &str = "rate_limit.invalid_max_rows";
    // Policy errors (TS throws these while the module loads).
    pub const INVALID_SCOPE: &str = "rate_limit.invalid_scope";
    pub const DUPLICATE_SCOPE: &str = "rate_limit.duplicate_scope";
    pub const INVALID_LIMIT: &str = "rate_limit.invalid_limit";
    pub const INVALID_WINDOW: &str = "rate_limit.invalid_window";
}

#[spacetimedb::table(accessor = rate_limit_bucket)]
#[derive(Clone)]
pub struct RateLimitBucket {
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub scope: String,
    #[index(btree)]
    pub window_start: Timestamp,
    #[index(btree)]
    pub expires_at: Timestamp,
    pub count: u32,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = rate_limit_admin_identity)]
pub struct RateLimitAdminIdentity {
    #[primary_key]
    pub identity: Identity,
    pub added_at: Timestamp,
}

#[spacetimedb::table(accessor = rate_limit_config, public)]
pub struct RateLimitConfig {
    #[primary_key]
    pub singleton: bool,
    pub sweep_batch: u32,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = rate_limit_sweep_tick, scheduled(rate_limit_sweep))]
pub struct RateLimitSweepTick {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

/// The outcome of [`RateLimitPolicy::consume`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimitResult {
    pub allowed: bool,
    pub key: String,
    pub scope: String,
    pub limit: u32,
    pub used: u32,
    pub remaining: u32,
    pub reset_at: Timestamp,
    /// 0 when allowed; otherwise whole seconds until the window resets (rounded up).
    pub retry_after_seconds: u32,
}

/// Current usage of one actor's bucket. `reset_at` is `None` when no window is open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimitStatus {
    pub limit: u32,
    pub window_seconds: u32,
    pub used: u32,
    pub remaining: u32,
    pub reset_at: Option<Timestamp>,
}

/// One scope's policy: at most `limit` units per `window_seconds`, per actor.
#[derive(Clone, Copy, Debug)]
pub struct RateLimitPolicy {
    pub scope: &'static str,
    pub limit: u32,
    pub window_seconds: u32,
}

/// Declare a policy (usually as a `const`).
pub const fn policy(scope: &'static str, limit: u32, window_seconds: u32) -> RateLimitPolicy {
    RateLimitPolicy {
        scope,
        limit,
        window_seconds,
    }
}

/// Lengths are UTF-16 code units, as in TS, so bucket keys match across languages.
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

thread_local! {
    /// Scopes seen in this module instance, with their policies.
    static REGISTERED: RefCell<HashMap<String, (u32, u32)>> = RefCell::new(HashMap::new());
}

impl RateLimitPolicy {
    /// Validate the policy and claim its scope (once per instance).
    fn check(&self) -> Result<(), String> {
        let len = utf16_len(self.scope);
        if len == 0 || len > MAX_SCOPE_LENGTH {
            return Err(errors::INVALID_SCOPE.into());
        }
        if self.limit == 0 {
            return Err(errors::INVALID_LIMIT.into());
        }
        if self.window_seconds == 0 {
            return Err(errors::INVALID_WINDOW.into());
        }
        REGISTERED.with_borrow_mut(|m| match m.get(self.scope) {
            Some(&existing) if existing != (self.limit, self.window_seconds) => {
                Err(errors::DUPLICATE_SCOPE.into())
            }
            Some(_) => Ok(()),
            None => {
                m.insert(self.scope.to_string(), (self.limit, self.window_seconds));
                Ok(())
            }
        })
    }

    /// The stored key: both parts length-prefixed so delimiters inside them can't collide.
    pub fn bucket_key(&self, actor_key: &str) -> Result<String, String> {
        let len = utf16_len(actor_key);
        if len == 0 || len > MAX_ACTOR_KEY_LENGTH {
            return Err(errors::INVALID_ACTOR_KEY.into());
        }
        Ok(format!(
            "{}:{}{}:{}",
            utf16_len(self.scope),
            self.scope,
            len,
            actor_key
        ))
    }

    /// Spend `cost` (usually 1) from the actor's bucket in the caller's transaction. A cost of
    /// 0 or above `limit` can never be allowed and fails with `rate_limit.invalid_cost`.
    pub fn consume(
        &self,
        ctx: &ReducerContext,
        actor_key: &str,
        cost: u32,
    ) -> Result<RateLimitResult, String> {
        self.check()?;
        let key = self.bucket_key(actor_key)?;
        if cost == 0 || cost > self.limit {
            return Err(errors::INVALID_COST.into());
        }
        let now = ctx.timestamp;
        let existing = ctx.db.rate_limit_bucket().key().find(&key);
        let open = existing.as_ref().filter(|b| b.expires_at > now);
        let used = open.map_or(0, |b| b.count);
        let reset_at = open.map_or(
            now + TimeDuration::from_micros(self.window_seconds as i64 * ONE_SECOND_MICROS),
            |b| b.expires_at,
        );

        if used as u64 + cost as u64 > self.limit as u64 {
            return Ok(RateLimitResult {
                allowed: false,
                key,
                scope: self.scope.into(),
                limit: self.limit,
                used,
                remaining: 0,
                reset_at,
                retry_after_seconds: seconds_until(now, reset_at),
            });
        }
        let row = RateLimitBucket {
            key: key.clone(),
            scope: self.scope.into(),
            window_start: open.map_or(now, |b| b.window_start),
            expires_at: reset_at,
            count: used + cost,
            updated_at: now,
        };
        // Update any existing row, including an expired one not yet swept.
        if existing.is_some() {
            ctx.db.rate_limit_bucket().key().update(row);
        } else {
            ctx.db.rate_limit_bucket().insert(row);
        }
        Ok(RateLimitResult {
            allowed: true,
            key,
            scope: self.scope.into(),
            limit: self.limit,
            used: used + cost,
            remaining: self.limit - used - cost,
            reset_at,
            retry_after_seconds: 0,
        })
    }

    /// Read the actor's bucket without spending. Views have no clock: pass `now` from a
    /// reducer or procedure to report an expired window as fresh; without it, compare
    /// `reset_at` against your own clock.
    pub fn peek(
        &self,
        db: &impl RateLimitRead,
        actor_key: &str,
        now: Option<Timestamp>,
    ) -> Result<RateLimitStatus, String> {
        self.check()?;
        let row = db.rate_limit_bucket_row(&self.bucket_key(actor_key)?);
        Ok(match row {
            Some(r) if now.is_none_or(|n| r.expires_at > n) => RateLimitStatus {
                limit: self.limit,
                window_seconds: self.window_seconds,
                used: r.count,
                remaining: self.limit.saturating_sub(r.count),
                reset_at: Some(r.expires_at),
            },
            _ => RateLimitStatus {
                limit: self.limit,
                window_seconds: self.window_seconds,
                used: 0,
                remaining: self.limit,
                reset_at: None,
            },
        })
    }
}

/// Read access to buckets, from a reducer or a view.
pub trait RateLimitRead {
    fn rate_limit_bucket_row(&self, key: &str) -> Option<RateLimitBucket>;
}
impl RateLimitRead for ReducerContext {
    fn rate_limit_bucket_row(&self, key: &str) -> Option<RateLimitBucket> {
        self.db.rate_limit_bucket().key().find(key.to_string())
    }
}
impl RateLimitRead for spacetimedb::TxContext {
    fn rate_limit_bucket_row(&self, key: &str) -> Option<RateLimitBucket> {
        self.db.rate_limit_bucket().key().find(key.to_string())
    }
}
impl RateLimitRead for ViewContext {
    fn rate_limit_bucket_row(&self, key: &str) -> Option<RateLimitBucket> {
        self.db.rate_limit_bucket().key().find(key.to_string())
    }
}

fn seconds_until(now: Timestamp, future: Timestamp) -> u32 {
    let delta = future.to_micros_since_unix_epoch() - now.to_micros_since_unix_epoch();
    if delta <= 0 {
        return 0;
    }
    ((delta + ONE_SECOND_MICROS - 1) / ONE_SECOND_MICROS).min(u32::MAX as i64) as u32
}

// ---------------------------------------------------------------------------
// Install, admin, sweep
// ---------------------------------------------------------------------------

/// Call from your `init` reducer: the publisher becomes the first admin, and the
/// expired-bucket sweep is scheduled every 30 s. Later calls do nothing.
pub fn install(ctx: &ReducerContext) {
    if ctx.db.rate_limit_config().singleton().find(true).is_some() {
        return;
    }
    ctx.db
        .rate_limit_admin_identity()
        .insert(RateLimitAdminIdentity {
            identity: ctx.sender(),
            added_at: ctx.timestamp,
        });
    ctx.db.rate_limit_config().insert(RateLimitConfig {
        singleton: true,
        sweep_batch: DEFAULT_SWEEP_BATCH,
        updated_at: ctx.timestamp,
    });
    ctx.db.rate_limit_sweep_tick().insert(RateLimitSweepTick {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(SWEEP_INTERVAL_MICROS)),
    });
}

pub fn is_admin(ctx: &ReducerContext, identity: Identity) -> bool {
    ctx.db
        .rate_limit_admin_identity()
        .identity()
        .find(identity)
        .is_some()
}

/// `Err(rate_limit.not_authorized)` unless the caller is a rate-limit admin.
pub fn require_admin(ctx: &ReducerContext) -> Result<(), String> {
    if is_admin(ctx, ctx.sender()) {
        Ok(())
    } else {
        Err(errors::NOT_AUTHORIZED.into())
    }
}

fn max_rows_arg(value: Option<u32>) -> Result<u32, String> {
    match value {
        None => Ok(DEFAULT_SWEEP_BATCH),
        Some(v) if (1..=MAX_SWEEP_BATCH).contains(&v) => Ok(v),
        Some(_) => Err(errors::INVALID_MAX_ROWS.into()),
    }
}

/// Delete up to `max_rows` expired buckets. Returns how many were deleted.
pub fn sweep(ctx: &ReducerContext, max_rows: u32) -> Result<u32, String> {
    if !(1..=MAX_SWEEP_BATCH).contains(&max_rows) {
        return Err(errors::INVALID_SWEEP_BATCH.into());
    }
    let expired: Vec<String> = ctx
        .db
        .rate_limit_bucket()
        .expires_at()
        .filter(Timestamp::from_micros_since_unix_epoch(i64::MIN)..=ctx.timestamp)
        .take(max_rows as usize)
        .map(|b| b.key)
        .collect();
    for key in &expired {
        ctx.db.rate_limit_bucket().key().delete(key);
    }
    Ok(expired.len() as u32)
}

/// Scheduled every 30 s by [`install`].
#[spacetimedb::reducer]
pub fn rate_limit_sweep(ctx: &ReducerContext, _tick: RateLimitSweepTick) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    let batch = ctx
        .db
        .rate_limit_config()
        .singleton()
        .find(true)
        .map_or(DEFAULT_SWEEP_BATCH, |c| c.sweep_batch);
    sweep(ctx, batch).map(|_| ())
}

/// Sweep now (admins). Returns how many buckets were deleted. TS: `run_sweep`.
#[spacetimedb::procedure]
pub fn rate_limit_run_sweep(
    ctx: &mut ProcedureContext,
    max_rows: Option<u32>,
) -> Result<u32, String> {
    let max_rows = max_rows_arg(max_rows)?;
    ctx.try_with_tx(|tx| {
        require_admin(tx)?;
        sweep(tx, max_rows)
    })
}

#[spacetimedb::reducer]
pub fn add_rate_limit_admin(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .rate_limit_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        ctx.db
            .rate_limit_admin_identity()
            .insert(RateLimitAdminIdentity {
                identity,
                added_at: ctx.timestamp,
            });
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn remove_rate_limit_admin(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .rate_limit_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        return Ok(());
    }
    if ctx.db.rate_limit_admin_identity().count() <= 1 {
        return Err(errors::CANNOT_REMOVE_LAST_ADMIN.into());
    }
    ctx.db
        .rate_limit_admin_identity()
        .identity()
        .delete(identity);
    Ok(())
}

/// Set how many expired buckets each scheduled sweep deletes (admins). TS: `update_config`.
#[spacetimedb::reducer]
pub fn rate_limit_update_config(ctx: &ReducerContext, sweep_batch: u32) -> Result<(), String> {
    require_admin(ctx)?;
    let cfg = ctx
        .db
        .rate_limit_config()
        .singleton()
        .find(true)
        .ok_or("rate_limit.config_missing")?;
    if !(1..=MAX_SWEEP_BATCH).contains(&sweep_batch) {
        return Err(errors::INVALID_SWEEP_BATCH.into());
    }
    ctx.db
        .rate_limit_config()
        .singleton()
        .update(RateLimitConfig {
            sweep_batch,
            updated_at: ctx.timestamp,
            ..cfg
        });
    Ok(())
}

/// Delete up to `max_rows` buckets, expired or not (admins). TS: `reset_buckets`.
#[spacetimedb::reducer]
pub fn rate_limit_reset_buckets(ctx: &ReducerContext, max_rows: Option<u32>) -> Result<(), String> {
    require_admin(ctx)?;
    let max_rows = max_rows_arg(max_rows)?;
    let keys: Vec<String> = ctx
        .db
        .rate_limit_bucket()
        .iter()
        .take(max_rows as usize)
        .map(|b| b.key)
        .collect();
    for key in &keys {
        ctx.db.rate_limit_bucket().key().delete(key);
    }
    Ok(())
}

/// Up to 1,000 buckets, for admins (others get nothing).
#[spacetimedb::view(accessor = admin_rate_limit_buckets, public)]
fn admin_rate_limit_buckets(ctx: &ViewContext) -> Vec<RateLimitBucket> {
    if ctx
        .db
        .rate_limit_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        return Vec::new();
    }
    // Views can't scan a table in 2.11, only read indexes: walk the whole expires_at index.
    ctx.db
        .rate_limit_bucket()
        .expires_at()
        .filter(Timestamp::from_micros_since_unix_epoch(i64::MIN)..)
        .take(1000)
        .collect()
}
