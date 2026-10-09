//! # retry.rs: retry failed background tasks with exponential backoff, for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/retry` (SpacetimeDB PR #5903, commit
//! `ae25f1da24b1723e28acd0038e75e598f33952c3`, package `spacetime-retry-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Same tables (`retry_task`, `retry_history`, `retry_admin_identity`), reducers
//! (`submit_retry_task`, `add_retry_admin_identity`, `remove_retry_admin_identity`), views
//! (`retry_tasks_admin`, `retry_history_admin`), error codes and backoff as the TS package.
//! The scheduled reducer is `retry_fire` (the TS example's name).
//!
//! Your crate root provides the task arguments and one handler:
//!
//! ```ignore
//! #[derive(spacetimedb::SpacetimeType, Clone)]
//! #[allow(non_camel_case_types)]
//! pub enum RetryArgs {            // one variant per task, named like the TS task tags
//!     send_receipt(SendReceiptArgs),
//!     cleanup,
//! }
//!
//! pub fn retry_handle(ctx: &ReducerContext, args: &RetryArgs) -> retry::RetryResult {
//!     match args {
//!         RetryArgs::send_receipt(a) => if send(ctx, a) { retry::ok() } else { retry::failed("smtp down") },
//!         RetryArgs::cleanup => retry::ok(),
//!     }
//! }
//!
//! #[spacetimedb::reducer(init)]
//! pub fn init(ctx: &ReducerContext) { retry::install(ctx); }
//! ```
//!
//! Changes from the TypeScript original:
//! - Handlers return `Err(RetryError::Failed(..))` to retry and `Err(RetryError::Permanent(..))`
//!   to give up (the TS `SenderError`). Rust can't catch panics in wasm: a panicking handler
//!   aborts the whole transaction, so that attempt leaves no history and schedules nothing.
//!   Return an error instead.
//! - `retry_fire` rejects calls from clients (scheduled reducers are client-callable in 2.11,
//!   and the TS version doesn't check, so any client could run any task with any arguments).
//! - [`run_attempt`] is public, for hosts that want to drive an attempt themselves.

use crate::RetryArgs;
use spacetimedb::{
    Identity, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration, Timestamp,
    ViewContext,
};

const ONE_SECOND_MICROS: i64 = 1_000_000;
pub const MAX_ATTEMPTS: u8 = 10;
pub const MAX_BACKOFF_SECONDS: u32 = 3600;
pub const MAX_TASK_NAME_LENGTH: usize = 128;
pub const MAX_ERROR_LENGTH: usize = 2048;
pub const MAX_ROWS: u64 = 1000;

/// The `retry.*` codes reducers fail with.
pub mod errors {
    pub const NOT_AUTHORIZED: &str = "retry.not_authorized";
    pub const INVALID_TASK_NAME: &str = "retry.invalid_task_name";
    pub const INVALID_MAX_ATTEMPTS: &str = "retry.invalid_max_attempts";
    pub const INVALID_BACKOFF_SECONDS: &str = "retry.invalid_backoff_seconds";
    /// Followed by `:` and the task name.
    pub const TASK_ALREADY_EXISTS: &str = "retry.task_already_exists";
    pub const CANNOT_REMOVE_LAST_ADMIN: &str = "retry.cannot_remove_last_admin";
}

/// Why an attempt didn't succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetryError {
    /// Try again after the backoff (TS: `retryFailed(error)` or a thrown `Error`).
    Failed(String),
    /// The arguments can never work; give up now (TS: a thrown `SenderError`).
    Permanent(String),
}

pub type RetryResult = Result<(), RetryError>;

pub fn ok() -> RetryResult {
    Ok(())
}
pub fn failed(error: impl Into<String>) -> RetryResult {
    Err(RetryError::Failed(error.into()))
}
pub fn permanent(error: impl Into<String>) -> RetryResult {
    Err(RetryError::Permanent(error.into()))
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryHistoryStatus {
    Ok,
    Failed,
    GaveUp,
}

#[spacetimedb::table(accessor = retry_task, scheduled(retry_fire))]
#[derive(Clone)]
pub struct RetryTask {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
    #[index(btree)]
    pub name: String,
    pub args: RetryArgs,
    pub attempt: u8,
    pub max_attempts: u8,
    pub backoff_secs: u32,
}

#[spacetimedb::table(accessor = retry_history)]
#[derive(Clone)]
pub struct RetryHistory {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub task_name: String,
    pub attempt: u8,
    pub status: RetryHistoryStatus,
    pub error: Option<String>,
    #[index(btree)]
    pub ran_at: Timestamp,
}

#[spacetimedb::table(accessor = retry_admin_identity)]
pub struct RetryAdminIdentity {
    #[primary_key]
    pub identity: Identity,
    pub added_at: Timestamp,
}

// ---------------------------------------------------------------------------
// Library API
// ---------------------------------------------------------------------------

/// Call from your `init` reducer: makes the publishing identity the first admin.
pub fn install(ctx: &ReducerContext) {
    add_admin(ctx, ctx.sender());
}

pub fn is_admin(ctx: &ReducerContext, who: Identity) -> bool {
    ctx.db.retry_admin_identity().identity().find(who).is_some()
}

/// `Err(retry.not_authorized)` unless the caller is a retry admin.
pub fn require_admin(ctx: &ReducerContext) -> Result<(), String> {
    if is_admin(ctx, ctx.sender()) {
        Ok(())
    } else {
        Err(errors::NOT_AUTHORIZED.into())
    }
}

fn add_admin(ctx: &ReducerContext, identity: Identity) {
    if ctx
        .db
        .retry_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        ctx.db.retry_admin_identity().insert(RetryAdminIdentity {
            identity,
            added_at: ctx.timestamp,
        });
    }
}

/// Validate a task and schedule its first attempt now. No authorization check: call it from
/// your own reducers after you've authorized the caller.
pub fn submit(
    ctx: &ReducerContext,
    name: String,
    args: RetryArgs,
    max_attempts: u8,
    backoff_secs: u32,
) -> Result<(), String> {
    // The TS limit is in UTF-16 code units; this counts chars.
    let len = name.chars().count();
    if len == 0 || len > MAX_TASK_NAME_LENGTH {
        return Err(errors::INVALID_TASK_NAME.into());
    }
    if !(1..=MAX_ATTEMPTS).contains(&max_attempts) {
        return Err(errors::INVALID_MAX_ATTEMPTS.into());
    }
    if !(1..=MAX_BACKOFF_SECONDS).contains(&backoff_secs) {
        return Err(errors::INVALID_BACKOFF_SECONDS.into());
    }
    if ctx.db.retry_task().name().filter(&name).next().is_some() {
        return Err(format!("{}:{name}", errors::TASK_ALREADY_EXISTS));
    }
    ctx.db.retry_task().insert(RetryTask {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Time(ctx.timestamp),
        name,
        args,
        attempt: 0,
        max_attempts,
        backoff_secs,
    });
    Ok(())
}

/// Run one attempt of `task` in the current transaction: call the handler, record history,
/// and schedule the next attempt if it failed and attempts remain. `retry_fire` calls this.
pub fn run_attempt(ctx: &ReducerContext, task: &RetryTask) {
    let result = crate::retry_handle(ctx, &task.args);
    let (status, error, retry_again) = match result {
        Ok(()) => (RetryHistoryStatus::Ok, None, false),
        Err(e) => {
            let (msg, permanent) = match e {
                RetryError::Failed(m) => (m, false),
                RetryError::Permanent(m) => (m, true),
            };
            let last = permanent || task.attempt as u32 + 1 >= task.max_attempts as u32;
            let status = if last {
                RetryHistoryStatus::GaveUp
            } else {
                RetryHistoryStatus::Failed
            };
            (
                status,
                Some(msg.chars().take(MAX_ERROR_LENGTH).collect()),
                !last,
            )
        }
    };

    // One history row per attempt; drop the oldest first so the table stays bounded.
    let history = ctx.db.retry_history();
    if history.count() >= MAX_ROWS
        && let Some(oldest) = oldest_history(ctx)
    {
        history.id().delete(oldest);
    }
    history.insert(RetryHistory {
        id: 0,
        task_name: task.name.clone(),
        attempt: task.attempt,
        status,
        error,
        ran_at: ctx.timestamp,
    });
    if !retry_again {
        return;
    }

    let delay = task.backoff_secs as i64 * (1i64 << task.attempt) * ONE_SECOND_MICROS;
    ctx.db.retry_task().insert(RetryTask {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Time(ctx.timestamp + TimeDuration::from_micros(delay)),
        attempt: task.attempt + 1,
        ..task.clone()
    });
}

/// The oldest history row by `ran_at`.
///
/// Within a transaction, a btree range scan yields the rows inserted by that transaction
/// first and the committed rows after them, each part in key order (SpacetimeDB 2.11). So the
/// minimum is either the first row, or the first row where the key goes down again.
fn oldest_history(ctx: &ReducerContext) -> Option<u64> {
    let mut rows = ctx
        .db
        .retry_history()
        .ran_at()
        .filter(Timestamp::from_micros_since_unix_epoch(i64::MIN)..);
    let first = rows.next()?;
    let mut prev = first.ran_at;
    for row in rows {
        if row.ran_at < prev {
            return Some(if row.ran_at < first.ran_at {
                row.id
            } else {
                first.id
            });
        }
        prev = row.ran_at;
    }
    Some(first.id)
}

// ---------------------------------------------------------------------------
// Reducers and views
// ---------------------------------------------------------------------------

/// The scheduled reducer that runs attempts.
#[spacetimedb::reducer]
pub fn retry_fire(ctx: &ReducerContext, arg: RetryTask) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    run_attempt(ctx, &arg);
    Ok(())
}

/// Submit a task (retry admins only).
#[spacetimedb::reducer]
pub fn submit_retry_task(
    ctx: &ReducerContext,
    name: String,
    args: RetryArgs,
    max_attempts: u8,
    backoff_secs: u32,
) -> Result<(), String> {
    require_admin(ctx)?;
    submit(ctx, name, args, max_attempts, backoff_secs)
}

#[spacetimedb::reducer]
pub fn add_retry_admin_identity(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    add_admin(ctx, identity);
    Ok(())
}

#[spacetimedb::reducer]
pub fn remove_retry_admin_identity(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .retry_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        return Ok(());
    }
    if ctx.db.retry_admin_identity().count() <= 1 {
        return Err(errors::CANNOT_REMOVE_LAST_ADMIN.into());
    }
    ctx.db.retry_admin_identity().identity().delete(identity);
    Ok(())
}

/// The latest pending tasks, newest first (retry admins only; others get nothing).
#[spacetimedb::view(accessor = retry_tasks_admin, public)]
fn retry_tasks_admin(ctx: &ViewContext) -> Vec<RetryTask> {
    if ctx
        .db
        .retry_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        return Vec::new();
    }
    // Views can't scan a table in 2.11, only read indexes, so walk the whole `name` index.
    let mut rows: Vec<RetryTask> = ctx.db.retry_task().name().filter(""..).collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.scheduled_id));
    rows.truncate(MAX_ROWS as usize);
    rows
}

/// Retained attempt history, newest first (retry admins only).
#[spacetimedb::view(accessor = retry_history_admin, public)]
fn retry_history_admin(ctx: &ViewContext) -> Vec<RetryHistory> {
    if ctx
        .db
        .retry_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        return Vec::new();
    }
    let mut rows: Vec<RetryHistory> = ctx
        .db
        .retry_history()
        .ran_at()
        .filter(Timestamp::from_micros_since_unix_epoch(i64::MIN)..)
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse((r.ran_at, r.id)));
    rows
}
