//! # cron.rs: calendar and interval jobs for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/cron` (SpacetimeDB PR #5903, commit
//! `ae25f1da24b1723e28acd0038e75e598f33952c3`, package `spacetime-cron-ts`, which uses
//! `cron-parser` 5.5.0). Licensed Apache-2.0, like the original. Original design and behaviour
//! by Clockwork Labs; this port is by Tinker (Gazz-Stripbolt).
//!
//! Declare your jobs once in the crate root, then schedule them from any reducer:
//!
//! ```ignore
//! pub mod cron;
//!
//! cron_jobs! {
//!     history_cap: 5,                 // completed runs kept per job
//!     reconcile_every_seconds: 0,     // 0 = no periodic lost-trigger repair
//!     jobs: {
//!         heartbeat => reducer,
//!         report(ReportArgs) => reducer,
//!         sync_feed => procedure,     // runs as a procedure (can make HTTP requests)
//!     }
//! }
//!
//! // Reducer jobs: runs inside the fire's transaction. Err rolls it back and records a failure.
//! pub fn cron_run(ctx: &ReducerContext, job: &CronJobArgsValue, inv: &cron::CronInvocation) -> Result<(), String> { .. }
//! // Procedure jobs: runs outside a transaction; use ctx.with_tx for database work.
//! pub fn cron_run_procedure(ctx: &mut ProcedureContext, job: &CronJobArgsValue, inv: &cron::CronInvocation) -> Result<(), String> { .. }
//!
//! #[spacetimedb::reducer(init)]
//! pub fn init(ctx: &ReducerContext) -> Result<(), String> {
//!     cron::schedule(ctx, CronJobArgsValue::heartbeat, cron::every(30), Default::default())
//! }
//! ```
//!
//! Same `cron_job` and `cron_run` tables, invocation ids (`job:generation:sequence`), failure
//! counting and auto-disable, generations, catch-up, checkpointing and lost-fire repair as the
//! TS package, and the same expression syntax (`cron-parser` 5.5.0, including `H`, `L`, `#`
//! and `@daily`-style aliases). Changes from the TypeScript original:
//! - Triggers live in two shared tables, `cron_fire` (reducer jobs, scheduled reducer
//!   `cron_tick`) and `cron_procedure_fire` (procedure jobs, scheduled procedure
//!   `cron_tick_procedure`), with one row per job. TS creates a `<job>_fire` table and a
//!   `<job>_cron` function per job, which needs code generation per job.
//! - Job arguments are a Rust enum, so `cron.missing_args` / `cron.unexpected_args` can't happen.
//! - Time zones come from a table embedded below, generated from IANA tzdata (see
//!   `TZDATA_VERSION`): each zone's current rule, plus explicit transitions from 2025 for the
//!   few zones that need them. Accurate for scheduling (future times); times before 2025 use
//!   the current rule. `UTC` and `±HH:MM` offsets work too. Zone names are case-insensitive.
//! - A handler that panics aborts the transaction like an `Err`, but can't schedule the
//!   failure-recovery call, so the job relies on reconciliation (`lost_fire`). Return `Err`.
//! - `publicTables` isn't supported (tables are private); expose [`public_jobs`] in a view.
//! - `cron_job` has an extra index on `enabled`, because views can only read indexes.

use spacetimedb::{
    ProcedureContext, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration, Timestamp,
    ViewContext,
};

use crate::CronJobArgsValue;

// ---------------------------------------------------------------------------
// Job declaration
// ---------------------------------------------------------------------------

/// How a job's handler runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CronJobKind {
    /// Inside the trigger's transaction (`crate::cron_run`).
    Reducer,
    /// As a procedure, outside a transaction (`crate::cron_run_procedure`).
    Procedure,
}

/// Implemented for `CronJobArgsValue` by [`cron_jobs!`].
pub trait CronArgs {
    /// The job this value belongs to (the enum variant's name).
    fn job_name(&self) -> &'static str;
}

/// Declares the module's jobs. See the module docs.
#[macro_export]
macro_rules! cron_jobs {
    (
        history_cap: $cap:expr,
        reconcile_every_seconds: $reconcile:expr,
        jobs: { $( $name:ident $( ( $args:ty ) )? => $kind:ident ),* $(,)? } $(,)?
    ) => {
        /// One variant per cron job, carrying the job's arguments.
        #[derive(spacetimedb::SpacetimeType, Clone, Debug, PartialEq)]
        #[allow(non_camel_case_types)]
        pub enum CronJobArgsValue {
            $( $name $( ( $args ) )? ),*
        }

        impl $crate::cron::CronArgs for CronJobArgsValue {
            fn job_name(&self) -> &'static str {
                match self {
                    $( CronJobArgsValue::$name { .. } => stringify!($name) ),*
                }
            }
        }

        /// Every job, with how it runs.
        pub const CRON_JOBS: &[(&str, $crate::cron::CronJobKind)] = &[
            $( (stringify!($name), $crate::cron_jobs!(@kind $kind)) ),*
        ];
        /// Completed runs kept per job (0-1000).
        pub const CRON_HISTORY_CAP: u32 = $cap;
        /// Periodic lost-trigger repair interval; 0 = off.
        pub const CRON_RECONCILE_EVERY_SECONDS: u32 = $reconcile;
    };
    (@kind reducer) => { $crate::cron::CronJobKind::Reducer };
    (@kind procedure) => { $crate::cron::CronJobKind::Procedure };
}

// ---------------------------------------------------------------------------
// Errors and limits
// ---------------------------------------------------------------------------

/// Codes a caller can receive. [`schedule`] returns the input codes; the disabled-reason codes
/// are stored on the job row. Some are followed by `:` and detail text.
pub mod errors {
    pub const NOT_AUTHORIZED: &str = "cron.not_authorized";
    pub const INVALID_MAX_FAILURES: &str = "cron.invalid_max_failures";
    pub const INVALID_INTERVAL: &str = "cron.invalid_interval";
    pub const INVALID_EXPRESSION: &str = "cron.invalid_expression";
    pub const INVALID_TIMEZONE: &str = "cron.invalid_timezone";
    pub const UNSATISFIABLE_EXPRESSION: &str = "cron.unsatisfiable_expression";
    pub const INVALID_JOB_NAME: &str = "cron.invalid_job_name";
    pub const INVALID_SCHEDULE_STATE: &str = "cron.invalid_schedule_state";
    pub const INVALID_ARGS_STATE: &str = "cron.invalid_args_state";
    pub const INVALID_TRIGGER: &str = "cron.invalid_trigger";
    pub const NO_FUTURE_OCCURRENCE: &str = "cron.no_future_occurrence";
    pub const INVALID_PROCEDURE_RECOVERY: &str = "cron.invalid_procedure_recovery";
}

const ONE_SECOND_MICROS: i64 = 1_000_000;
pub const MAX_INTERVAL_SECONDS: u32 = 31_536_000;
pub const MAX_HISTORY_CAP: u32 = 1_000;
pub const MAX_ERROR_LENGTH: usize = 1_024;
pub const MAX_CRON_EXPRESSION_LENGTH: usize = 256;
pub const MAX_TIMEZONE_LENGTH: usize = 128;
const MAX_JOB_NAME_LENGTH: usize = 48;
/// The host can schedule about 795 days ahead; sparse schedules re-arm yearly.
const CHECKPOINT_DELAY_MICROS: i64 = 365 * 24 * 60 * 60 * ONE_SECOND_MICROS;
const RECONCILE_TICK_KEY: &str = "cron";
/// JS `Date`'s range, which bounds the original's answers.
const MAX_SAFE_DATE_MS: i64 = 8_640_000_000_000_000;

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

#[derive(SpacetimeType, Clone, Debug, PartialEq, Eq)]
pub struct CronSpec {
    pub expression: String,
    pub timezone: String,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq, Eq)]
pub struct EverySpec {
    pub seconds: u32,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum CronSchedule {
    cron(CronSpec),
    every(EverySpec),
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CronRunStatus {
    Ok,
    Failed,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq, Eq)]
pub struct CronFireRecovery {
    pub sequence: u64,
    pub scheduled_for: Timestamp,
    pub error: String,
}

#[spacetimedb::table(accessor = cron_job)]
#[derive(Clone)]
pub struct CronJob {
    #[primary_key]
    pub name: String,
    pub schedule: CronSchedule,
    pub args: CronJobArgsValue,
    #[index(btree)]
    pub enabled: bool,
    pub max_failures: u32,
    pub consecutive_failures: u32,
    pub fire_count: u64,
    pub generation: u64,
    pub last_run_at: Option<Timestamp>,
    pub next_run_at: Option<Timestamp>,
    pub disabled_reason: Option<String>,
}

#[spacetimedb::table(accessor = cron_run)]
#[derive(Clone)]
pub struct CronRun {
    #[primary_key]
    pub invocation_id: String,
    #[index(btree)]
    pub job_name: String,
    pub generation: u64,
    pub sequence: u64,
    pub scheduled_for: Timestamp,
    pub completed_at: Timestamp,
    pub status: CronRunStatus,
    pub error: Option<String>,
}

/// Pending triggers of reducer jobs (one per job).
#[spacetimedb::table(accessor = cron_fire, scheduled(cron_tick))]
#[derive(Clone)]
pub struct CronFire {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
    #[unique]
    pub job_name: String,
    pub generation: u64,
    pub target_at: Option<Timestamp>,
    pub recovery: Option<CronFireRecovery>,
}

/// Pending triggers of procedure jobs (one per job). Same columns as `cron_fire`.
#[spacetimedb::table(accessor = cron_procedure_fire, scheduled(cron_tick_procedure))]
#[derive(Clone)]
pub struct CronProcedureFire {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
    #[unique]
    pub job_name: String,
    pub generation: u64,
    pub target_at: Option<Timestamp>,
    pub recovery: Option<CronFireRecovery>,
}

#[spacetimedb::table(accessor = cron_reconcile_tick, scheduled(cron_reconcile))]
pub struct CronReconcileTick {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
    #[unique]
    pub key: String,
}

/// Public projection of `cron_job`, without arguments (see [`public_jobs`]).
#[derive(SpacetimeType, Clone, Debug)]
pub struct CronJobView {
    pub name: String,
    pub schedule: CronSchedule,
    pub enabled: bool,
    pub max_failures: u32,
    pub consecutive_failures: u32,
    pub fire_count: u64,
    pub generation: u64,
    pub last_run_at: Option<Timestamp>,
    pub next_run_at: Option<Timestamp>,
    pub disabled_reason: Option<String>,
}

/// Stable metadata passed to every invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronInvocation {
    /// Unique across every generation of every job: `job:generation:sequence`.
    pub id: String,
    pub job_name: String,
    pub generation: u64,
    pub sequence: u64,
    /// The logical calendar occurrence or interval fire time.
    pub scheduled_for: Timestamp,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A cron expression or a fixed interval.
#[derive(Clone, Debug)]
pub enum ScheduleSpec {
    Cron(String),
    Every(u32),
}

/// Every `seconds` seconds (1 to 31,536,000).
pub fn every(seconds: u32) -> ScheduleSpec {
    ScheduleSpec::Every(seconds)
}

/// A cron expression (5 or 6 fields, or an alias like `@daily`).
pub fn cron(expression: impl Into<String>) -> ScheduleSpec {
    ScheduleSpec::Cron(expression.into())
}

#[derive(Clone, Debug, Default)]
pub struct ScheduleOpts {
    /// IANA time zone for cron expressions. Defaults to `UTC`.
    pub timezone: Option<String>,
    /// Consecutive failures before the job disables itself. `None`/`0` = never.
    pub max_failures: Option<u32>,
}

/// Create or replace a job's schedule and arm its first trigger. The job is the variant of
/// `args`. A new generation starts: pending triggers from the old schedule are dropped.
pub fn schedule(
    ctx: &ReducerContext,
    args: CronJobArgsValue,
    spec: ScheduleSpec,
    opts: ScheduleOpts,
) -> Result<(), String> {
    use CronArgs as _;
    let job_name = args.job_name();
    let kind = job_kind(job_name)?;
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let (schedule, first_at) = normalize_schedule(&spec, opts.timezone.as_deref(), now)?;
    let max_failures = opts.max_failures.unwrap_or(0);

    ensure_reconcile_tick(ctx);
    reconcile_lost_fires(ctx);

    let existing = ctx.db.cron_job().name().find(job_name.to_string());
    let generation = existing.as_ref().map_or(0, |r| r.generation) + 1;
    disarm_fire(ctx, kind, job_name);
    let mut row = CronJob {
        name: job_name.to_string(),
        schedule,
        args,
        enabled: true,
        max_failures,
        consecutive_failures: 0,
        fire_count: existing.as_ref().map_or(0, |r| r.fire_count),
        generation,
        last_run_at: existing.as_ref().and_then(|r| r.last_run_at),
        next_run_at: None,
        disabled_reason: None,
    };
    if existing.is_some() {
        ctx.db.cron_job().name().update(row.clone());
    } else {
        ctx.db.cron_job().insert(row.clone());
    }
    let next = insert_fire(ctx, kind, &row, first_at).ok_or(errors::UNSATISFIABLE_EXPRESSION)?;
    row.next_run_at = Some(next);
    ctx.db.cron_job().name().update(row);
    Ok(())
}

/// Disable a job and remove its pending trigger.
pub fn unschedule(ctx: &ReducerContext, job_name: &str) -> Result<(), String> {
    let kind = job_kind(job_name)?;
    ensure_reconcile_tick(ctx);
    reconcile_lost_fires(ctx);
    let Some(row) = ctx.db.cron_job().name().find(job_name.to_string()) else {
        return Ok(());
    };
    disarm_fire(ctx, kind, job_name);
    ctx.db.cron_job().name().update(CronJob {
        enabled: false,
        generation: row.generation + 1,
        consecutive_failures: 0,
        next_run_at: None,
        disabled_reason: Some("disabled_by_operator".into()),
        ..row
    });
    Ok(())
}

/// Sanitized job state for a public view (no arguments; internal error text mapped to a
/// short reason). TS: `cron.publicViews(spacetimedb).jobs`.
///
/// ```ignore
/// #[spacetimedb::view(accessor = cron_jobs, public)]
/// fn cron_jobs(ctx: &ViewContext) -> Vec<cron::CronJobView> { cron::public_jobs(ctx) }
/// ```
pub fn public_jobs(ctx: &ViewContext) -> Vec<CronJobView> {
    let mut rows: Vec<CronJob> = ctx.db.cron_job().enabled().filter(true).collect();
    rows.extend(ctx.db.cron_job().enabled().filter(false));
    rows.into_iter()
        .map(|r| CronJobView {
            name: r.name,
            schedule: r.schedule,
            enabled: r.enabled,
            max_failures: r.max_failures,
            consecutive_failures: r.consecutive_failures,
            fire_count: r.fire_count,
            generation: r.generation,
            last_run_at: r.last_run_at,
            next_run_at: r.next_run_at,
            disabled_reason: public_disabled_reason(r.disabled_reason.as_deref()),
        })
        .collect()
}

/// Map a stored disabled reason to the public vocabulary.
pub fn public_disabled_reason(reason: Option<&str>) -> Option<String> {
    let r = reason?;
    let out = if r == "disabled_by_operator" {
        "disabled_by_operator"
    } else if r.starts_with(&format!("{}:", errors::INVALID_SCHEDULE_STATE)) {
        "invalid_schedule_state"
    } else if let Some(rest) = consecutive_failure_reason(r) {
        if rest == "lost_fire" {
            "lost_fire_threshold_reached"
        } else {
            "failure_threshold_reached"
        }
    } else {
        "disabled"
    };
    Some(out.into())
}

/// `failed_<n>_consecutive_times:<rest>` with n ≥ 1 → `Some(rest)`.
fn consecutive_failure_reason(r: &str) -> Option<&str> {
    let n = r.strip_prefix("failed_")?;
    let digits = n.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || n.as_bytes()[0] == b'0' {
        return None;
    }
    n[digits..].strip_prefix("_consecutive_times:")
}

// ---------------------------------------------------------------------------
// Scheduled functions
// ---------------------------------------------------------------------------

/// Fires reducer jobs.
#[spacetimedb::reducer]
pub fn cron_tick(ctx: &ReducerContext, arg: CronFire) -> Result<(), String> {
    require_database_caller(ctx)?;
    let Some((_, CronJobKind::Reducer)) = job_spec(&arg.job_name) else {
        return Ok(());
    };
    if arg.recovery.is_some() {
        recover_failure(ctx, &FireRow::from(&arg));
        return Ok(());
    }
    let Some((args, invocation)) =
        prepare_fire(ctx, CronJobKind::Reducer, &FireRow::from(&arg), false)
    else {
        return Ok(());
    };
    if let Err(error) = crate::cron_run(ctx, &args, &invocation) {
        let detail = truncate_error(&error);
        // Best effort: record the failure in a new transaction once this one rolls back.
        // Reconciliation repairs a lost calendar trigger if the host drops this call.
        let recovery = CronFire {
            recovery: Some(CronFireRecovery {
                sequence: invocation.sequence,
                scheduled_for: invocation.scheduled_for,
                error: detail,
            }),
            ..arg
        };
        spacetimedb::volatile_nonatomic_schedule_immediate!(cron_tick(recovery));
        return Err(error);
    }
    apply_outcome(
        ctx,
        CronJobKind::Reducer,
        &invocation,
        CronRunStatus::Ok,
        None,
        true,
    );
    Ok(())
}

/// Fires procedure jobs: reserve the sequence, run the handler outside a transaction, then
/// record the outcome.
///
/// Returns unit like the TS version; a caller other than the scheduler gets an error (panic).
#[spacetimedb::procedure]
pub fn cron_tick_procedure(ctx: &mut ProcedureContext, arg: CronProcedureFire) {
    if ctx.sender() != ctx.database_identity() {
        panic!("{}", errors::NOT_AUTHORIZED);
    }
    if arg.recovery.is_some() {
        panic!("{}", errors::INVALID_PROCEDURE_RECOVERY);
    }
    let fire = FireRow::from(&arg);
    let Some((args, invocation)) =
        ctx.with_tx(|tx| prepare_fire(tx, CronJobKind::Procedure, &fire, true))
    else {
        return;
    };
    let error = crate::cron_run_procedure(ctx, &args, &invocation)
        .err()
        .map(|e| truncate_error(&e));
    ctx.with_tx(|tx| {
        let status = if error.is_none() {
            CronRunStatus::Ok
        } else {
            CronRunStatus::Failed
        };
        apply_outcome(
            tx,
            CronJobKind::Procedure,
            &invocation,
            status,
            error.clone(),
            false,
        );
    });
}

/// Optional periodic repair of enabled jobs whose trigger went missing.
#[spacetimedb::reducer]
pub fn cron_reconcile(ctx: &ReducerContext, _tick: CronReconcileTick) -> Result<(), String> {
    require_database_caller(ctx)?;
    reconcile_lost_fires(ctx);
    Ok(())
}

// ---------------------------------------------------------------------------
// Engine (a port of cron.ts)
// ---------------------------------------------------------------------------

fn job_spec(name: &str) -> Option<(&'static str, CronJobKind)> {
    crate::CRON_JOBS.iter().copied().find(|(n, _)| *n == name)
}

fn job_kind(name: &str) -> Result<CronJobKind, String> {
    validate_job_name(name)?;
    job_spec(name)
        .map(|(_, k)| k)
        .ok_or_else(|| format!("cron.unknown_job:{name}"))
}

fn validate_job_name(name: &str) -> Result<(), String> {
    // /^[a-z][a-z0-9]*(?:_[a-z0-9]+)*$/, at most 48 characters.
    let ok = !name.is_empty()
        && name.len() <= MAX_JOB_NAME_LENGTH
        && name.as_bytes()[0].is_ascii_lowercase()
        && name.split('_').all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{}:use 1-48 lowercase snake_case characters",
            errors::INVALID_JOB_NAME
        ))
    }
}

fn require_database_caller(ctx: &ReducerContext) -> Result<(), String> {
    if ctx.sender() == ctx.database_identity() {
        Ok(())
    } else {
        Err(errors::NOT_AUTHORIZED.into())
    }
}

fn invocation_id(job: &str, generation: u64, sequence: u64) -> String {
    format!("{job}:{generation}:{sequence}")
}

pub fn truncate_error(message: &str) -> String {
    // The TS limit counts UTF-16 code units; this counts chars.
    if message.chars().count() <= MAX_ERROR_LENGTH {
        message.to_string()
    } else {
        let mut s: String = message.chars().take(MAX_ERROR_LENGTH - 3).collect();
        s.push_str("...");
        s
    }
}

/// The columns both fire tables share.
#[derive(Clone)]
struct FireRow {
    scheduled_id: u64,
    job_name: String,
    generation: u64,
    target_at: Option<Timestamp>,
    recovery: Option<CronFireRecovery>,
}

impl From<&CronFire> for FireRow {
    fn from(f: &CronFire) -> Self {
        FireRow {
            scheduled_id: f.scheduled_id,
            job_name: f.job_name.clone(),
            generation: f.generation,
            target_at: f.target_at,
            recovery: f.recovery.clone(),
        }
    }
}
impl From<&CronProcedureFire> for FireRow {
    fn from(f: &CronProcedureFire) -> Self {
        FireRow {
            scheduled_id: f.scheduled_id,
            job_name: f.job_name.clone(),
            generation: f.generation,
            target_at: f.target_at,
            recovery: f.recovery.clone(),
        }
    }
}

fn find_fire(ctx: &ReducerContext, kind: CronJobKind, job: &str) -> Option<FireRow> {
    match kind {
        CronJobKind::Reducer => ctx
            .db
            .cron_fire()
            .job_name()
            .find(job.to_string())
            .as_ref()
            .map(FireRow::from),
        CronJobKind::Procedure => ctx
            .db
            .cron_procedure_fire()
            .job_name()
            .find(job.to_string())
            .as_ref()
            .map(FireRow::from),
    }
}

fn delete_fire(ctx: &ReducerContext, kind: CronJobKind, scheduled_id: u64) {
    match kind {
        CronJobKind::Reducer => {
            ctx.db.cron_fire().scheduled_id().delete(scheduled_id);
        }
        CronJobKind::Procedure => {
            ctx.db
                .cron_procedure_fire()
                .scheduled_id()
                .delete(scheduled_id);
        }
    }
}

fn disarm_fire(ctx: &ReducerContext, kind: CronJobKind, job: &str) {
    if let Some(f) = find_fire(ctx, kind, job) {
        delete_fire(ctx, kind, f.scheduled_id);
    }
}

fn insert_fire_row(
    ctx: &ReducerContext,
    kind: CronJobKind,
    scheduled_at: ScheduleAt,
    job: &str,
    generation: u64,
    target_at: Option<Timestamp>,
) {
    match kind {
        CronJobKind::Reducer => {
            ctx.db.cron_fire().insert(CronFire {
                scheduled_id: 0,
                scheduled_at,
                job_name: job.into(),
                generation,
                target_at,
                recovery: None,
            });
        }
        CronJobKind::Procedure => {
            ctx.db.cron_procedure_fire().insert(CronProcedureFire {
                scheduled_id: 0,
                scheduled_at,
                job_name: job.into(),
                generation,
                target_at,
                recovery: None,
            });
        }
    }
}

fn ts(micros: i64) -> Timestamp {
    Timestamp::from_micros_since_unix_epoch(micros)
}

fn bounded_schedule_time(now: i64, target: i64) -> i64 {
    target.min(now.saturating_add(CHECKPOINT_DELAY_MICROS))
}

/// Arm the next trigger. Interval jobs get a repeating interval schedule; calendar jobs a
/// one-shot (checkpointed at most a year out). Returns the next run time.
fn insert_fire(
    ctx: &ReducerContext,
    kind: CronJobKind,
    row: &CronJob,
    target_micros: Option<i64>,
) -> Option<Timestamp> {
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    match &row.schedule {
        CronSchedule::every(e) => {
            let every = e.seconds as i64 * ONE_SECOND_MICROS;
            insert_fire_row(
                ctx,
                kind,
                ScheduleAt::Interval(TimeDuration::from_micros(every)),
                &row.name,
                row.generation,
                None,
            );
            Some(ts(now + every))
        }
        CronSchedule::cron(_) => {
            let target = target_micros?;
            let at = ScheduleAt::Time(ts(bounded_schedule_time(now, target)));
            insert_fire_row(ctx, kind, at, &row.name, row.generation, Some(ts(target)));
            Some(ts(target))
        }
    }
}

fn next_occurrence(schedule: &CronSchedule, after_micros: i64) -> Result<Option<i64>, String> {
    match schedule {
        CronSchedule::every(e) => Ok(Some(after_micros + e.seconds as i64 * ONE_SECOND_MICROS)),
        CronSchedule::cron(c) => {
            let parsed = parse_cron_expression(&c.expression)?;
            Ok(next_fire_after(&parsed, after_micros, &c.timezone))
        }
    }
}

fn replace_fire(ctx: &ReducerContext, kind: CronJobKind, row: CronJob) -> CronJob {
    disarm_fire(ctx, kind, &row.name);
    let mut target = None;
    if matches!(row.schedule, CronSchedule::cron(_)) {
        match next_occurrence(&row.schedule, ctx.timestamp.to_micros_since_unix_epoch()) {
            Err(e) => {
                return disable_job(
                    ctx,
                    kind,
                    row,
                    &format!("{}:{}", errors::INVALID_SCHEDULE_STATE, truncate_error(&e)),
                );
            }
            Ok(None) => return disable_job(ctx, kind, row, errors::NO_FUTURE_OCCURRENCE),
            Ok(t) => target = t,
        }
    }
    let next_run_at = insert_fire(ctx, kind, &row, target);
    let updated = CronJob { next_run_at, ..row };
    ctx.db.cron_job().name().update(updated.clone());
    updated
}

fn ensure_reconcile_tick(ctx: &ReducerContext) {
    let every = crate::CRON_RECONCILE_EVERY_SECONDS;
    if every == 0
        || ctx
            .db
            .cron_reconcile_tick()
            .key()
            .find(RECONCILE_TICK_KEY.to_string())
            .is_some()
    {
        return;
    }
    ctx.db.cron_reconcile_tick().insert(CronReconcileTick {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(
            every as i64 * ONE_SECOND_MICROS,
        )),
        key: RECONCILE_TICK_KEY.into(),
    });
}

fn reconcile_lost_fires(ctx: &ReducerContext) {
    use CronArgs as _;
    for &(name, kind) in crate::CRON_JOBS {
        let row = ctx.db.cron_job().name().find(name.to_string());
        let Some(row) = row.filter(|r| r.enabled) else {
            disarm_fire(ctx, kind, name);
            continue;
        };
        if row.args.job_name() != name {
            disable_job(ctx, kind, row, errors::INVALID_ARGS_STATE);
            continue;
        }
        let pending = find_fire(ctx, kind, name);
        let is_cron = matches!(row.schedule, CronSchedule::cron(_));
        let valid = pending.as_ref().is_some_and(|p| {
            p.generation == row.generation
                && p.recovery.is_none()
                && p.target_at.is_some() == is_cron
        });
        if valid {
            continue;
        }
        let sequence = row.fire_count + 1;
        let invocation = CronInvocation {
            id: invocation_id(name, row.generation, sequence),
            job_name: name.into(),
            generation: row.generation,
            sequence,
            scheduled_for: row.next_run_at.unwrap_or(ctx.timestamp),
        };
        replace_fire(ctx, kind, row);
        apply_outcome(
            ctx,
            kind,
            &invocation,
            CronRunStatus::Failed,
            Some("lost_fire".into()),
            true,
        );
    }
}

fn disable_job(ctx: &ReducerContext, kind: CronJobKind, row: CronJob, reason: &str) -> CronJob {
    disarm_fire(ctx, kind, &row.name);
    let updated = CronJob {
        enabled: false,
        next_run_at: None,
        disabled_reason: Some(truncate_error(reason)),
        ..row
    };
    ctx.db.cron_job().name().update(updated.clone());
    updated
}

fn apply_outcome(
    ctx: &ReducerContext,
    kind: CronJobKind,
    invocation: &CronInvocation,
    status: CronRunStatus,
    error: Option<String>,
    advance_fire_count: bool,
) {
    if ctx
        .db
        .cron_run()
        .invocation_id()
        .find(invocation.id.clone())
        .is_some()
    {
        return;
    }
    if let Some(row) = ctx.db.cron_job().name().find(invocation.job_name.clone())
        && row.generation == invocation.generation
    {
        if advance_fire_count && invocation.sequence != row.fire_count + 1 {
            return;
        }
        let consecutive_failures = if status == CronRunStatus::Failed {
            row.consecutive_failures.saturating_add(1)
        } else {
            0
        };
        let row = CronJob {
            fire_count: invocation.sequence.max(row.fire_count),
            consecutive_failures,
            last_run_at: Some(ctx.timestamp),
            ..row
        };
        ctx.db.cron_job().name().update(row.clone());
        if status == CronRunStatus::Failed
            && row.enabled
            && row.max_failures > 0
            && consecutive_failures >= row.max_failures
        {
            let detail = error.as_deref().unwrap_or("Failed");
            disable_job(
                ctx,
                kind,
                row,
                &format!("failed_{consecutive_failures}_consecutive_times:{detail}"),
            );
        }
    }
    record_run(ctx, invocation, status, error);
}

fn record_run(
    ctx: &ReducerContext,
    invocation: &CronInvocation,
    status: CronRunStatus,
    error: Option<String>,
) {
    let cap = crate::CRON_HISTORY_CAP.min(MAX_HISTORY_CAP) as usize;
    if cap == 0
        || ctx
            .db
            .cron_run()
            .invocation_id()
            .find(invocation.id.clone())
            .is_some()
    {
        return;
    }
    ctx.db.cron_run().insert(CronRun {
        invocation_id: invocation.id.clone(),
        job_name: invocation.job_name.clone(),
        generation: invocation.generation,
        sequence: invocation.sequence,
        scheduled_for: invocation.scheduled_for,
        completed_at: ctx.timestamp,
        status,
        error,
    });
    let mut rows: Vec<CronRun> = ctx
        .db
        .cron_run()
        .job_name()
        .filter(&invocation.job_name)
        .collect();
    rows.sort_by_key(|r| r.sequence);
    let remove = rows.len().saturating_sub(cap);
    for r in rows.into_iter().take(remove) {
        ctx.db.cron_run().invocation_id().delete(r.invocation_id);
    }
}

/// Work out the invocation for a trigger, re-arm the next one, and return the job's
/// arguments. `None` when the trigger is stale or the job can't run.
fn prepare_fire(
    ctx: &ReducerContext,
    kind: CronJobKind,
    arg: &FireRow,
    reserve_sequence: bool,
) -> Option<(CronJobArgsValue, CronInvocation)> {
    use CronArgs as _;
    let mut row = ctx.db.cron_job().name().find(arg.job_name.clone())?;
    if !row.enabled || row.generation != arg.generation {
        return None;
    }
    if row.args.job_name() != arg.job_name {
        disable_job(ctx, kind, row, errors::INVALID_ARGS_STATE);
        return None;
    }
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let mut scheduled_for = ctx.timestamp;
    match &row.schedule {
        CronSchedule::cron(_) => {
            let Some(target_at) = arg.target_at else {
                disable_job(
                    ctx,
                    kind,
                    row,
                    &format!("{}:missing_target", errors::INVALID_TRIGGER),
                );
                return None;
            };
            delete_fire(ctx, kind, arg.scheduled_id);
            let target = target_at.to_micros_since_unix_epoch();
            if target > now {
                // A yearly checkpoint woke us before the occurrence: re-arm for it.
                let next_run_at = insert_fire(ctx, kind, &row, Some(target));
                ctx.db
                    .cron_job()
                    .name()
                    .update(CronJob { next_run_at, ..row });
                return None;
            }
            scheduled_for = target_at;
            match next_occurrence(&row.schedule, now) {
                Ok(Some(next)) => {
                    row.next_run_at = insert_fire(ctx, kind, &row, Some(next));
                    ctx.db.cron_job().name().update(row.clone());
                }
                _ => row = disable_job(ctx, kind, row, errors::NO_FUTURE_OCCURRENCE),
            }
        }
        CronSchedule::every(e) => {
            row.next_run_at = Some(ts(now + e.seconds as i64 * ONE_SECOND_MICROS));
            ctx.db.cron_job().name().update(row.clone());
        }
    }
    let sequence = row.fire_count + 1;
    if reserve_sequence {
        row.fire_count = sequence;
        ctx.db.cron_job().name().update(row.clone());
    }
    let invocation = CronInvocation {
        id: invocation_id(&row.name, row.generation, sequence),
        job_name: row.name.clone(),
        generation: row.generation,
        sequence,
        scheduled_for,
    };
    Some((row.args, invocation))
}

fn recover_failure(ctx: &ReducerContext, arg: &FireRow) {
    let Some(recovery) = &arg.recovery else {
        return;
    };
    let kind = CronJobKind::Reducer;
    let Some(mut row) = ctx.db.cron_job().name().find(arg.job_name.clone()) else {
        return;
    };
    if !row.enabled || row.generation != arg.generation || recovery.sequence != row.fire_count + 1 {
        return;
    }
    match row.schedule {
        // Replace the trigger unconditionally so stale state can't block recovery.
        CronSchedule::cron(_) => {
            replace_fire(ctx, kind, row);
        }
        CronSchedule::every(ref e) => {
            row.next_run_at =
                Some(ts(ctx.timestamp.to_micros_since_unix_epoch()
                    + e.seconds as i64 * ONE_SECOND_MICROS));
            ctx.db.cron_job().name().update(row);
        }
    }
    let invocation = CronInvocation {
        id: invocation_id(&arg.job_name, arg.generation, recovery.sequence),
        job_name: arg.job_name.clone(),
        generation: arg.generation,
        sequence: recovery.sequence,
        scheduled_for: recovery.scheduled_for,
    };
    apply_outcome(
        ctx,
        kind,
        &invocation,
        CronRunStatus::Failed,
        Some(truncate_error(&recovery.error)),
        true,
    );
}

/// Validate a schedule and compute its first occurrence after `now`.
fn normalize_schedule(
    spec: &ScheduleSpec,
    timezone: Option<&str>,
    now: i64,
) -> Result<(CronSchedule, Option<i64>), String> {
    match spec {
        ScheduleSpec::Every(seconds) => {
            if *seconds < 1 || *seconds > MAX_INTERVAL_SECONDS {
                return Err(format!(
                    "{}:seconds must be an integer between 1 and {MAX_INTERVAL_SECONDS}",
                    errors::INVALID_INTERVAL
                ));
            }
            Ok((
                CronSchedule::every(EverySpec { seconds: *seconds }),
                Some(now + *seconds as i64 * ONE_SECOND_MICROS),
            ))
        }
        ScheduleSpec::Cron(expr) => {
            let expression = expr.trim();
            let tz = timezone.unwrap_or("UTC").trim();
            let tz = if tz.is_empty() { "UTC" } else { tz };
            if expression.is_empty() {
                return Err(format!("{}:empty", errors::INVALID_EXPRESSION));
            }
            if utf16_len(expression) > MAX_CRON_EXPRESSION_LENGTH {
                return Err(format!("{}:too_long", errors::INVALID_EXPRESSION));
            }
            if utf16_len(tz) > MAX_TIMEZONE_LENGTH {
                return Err(format!("{}:too_long", errors::INVALID_TIMEZONE));
            }
            if !is_valid_timezone(tz) {
                return Err(format!("{}:{tz}", errors::INVALID_TIMEZONE));
            }
            let parsed = parse_cron_expression(expression)
                .map_err(|e| format!("{}:{e}", errors::INVALID_EXPRESSION))?;
            let first =
                next_fire_after(&parsed, now, tz).ok_or(errors::UNSATISFIABLE_EXPRESSION)?;
            Ok((
                CronSchedule::cron(CronSpec {
                    expression: expression.into(),
                    timezone: tz.into(),
                }),
                Some(first),
            ))
        }
    }
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

// ---------------------------------------------------------------------------
// Cron expressions (a port of cron-parser 5.5.0's parsing and matching)
// ---------------------------------------------------------------------------

/// The `hashSeed` the TS package passes to cron-parser, so `H` resolves identically.
const HASH_SEED: &str = "spacetimedb-cron-submodule";

/// A parsed cron expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCron {
    seconds: [bool; 60],
    minutes: [bool; 60],
    hours: [bool; 24],
    months: [bool; 13],
    dom: [bool; 32],
    dom_last: bool,
    dom_wildcard: bool,
    dow: [bool; 7],
    /// Weekdays from `nL` entries (last such weekday of the month).
    dow_last: [bool; 7],
    dow_has_last: bool,
    /// A bare `L` in day-of-week: cron-parser fails when it tries to match it.
    dow_last_invalid: bool,
    dow_wildcard: bool,
    nth: u8,
}

/// cron-parser's seeded PRNG: FNV-1a of the seed into mulberry32.
struct Mulberry32(u32);
impl Mulberry32 {
    fn seeded(seed: &str) -> Self {
        let mut h: u32 = 2166136261;
        for unit in seed.encode_utf16() {
            h ^= unit as u32;
            h = h.wrapping_mul(16777619);
        }
        Mulberry32(h)
    }
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b79f5);
        let mut t = self.0;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        ((t ^ (t >> 14)) as f64) / 4294967296.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Second,
    Minute,
    Hour,
    DayOfMonth,
    Month,
    DayOfWeek,
}

impl Unit {
    fn bounds(self) -> (i64, i64) {
        match self {
            Unit::Second | Unit::Minute => (0, 59),
            Unit::Hour => (0, 23),
            Unit::DayOfMonth => (1, 31),
            Unit::Month => (1, 12),
            Unit::DayOfWeek => (0, 7),
        }
    }
    fn allows_l(self) -> bool {
        matches!(self, Unit::DayOfMonth | Unit::DayOfWeek)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum FieldValue {
    Num(i64),
    Str(String),
}

/// JS `parseInt(s, 10)`.
fn js_parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let n = rest.bytes().take_while(u8::is_ascii_digit).count();
    if n == 0 {
        return None;
    }
    let v = rest[..n].parse::<i64>().unwrap_or(i64::MAX);
    Some(if neg { -v } else { v })
}

/// JS unary `+s` for the strings that can reach it (digits, letters, `L`).
fn js_number(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() {
        return Some(0);
    }
    if t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse().ok();
    }
    None
}

/// The `validChars` regexes of cron-parser's fields.
fn valid_chars(unit: Unit, value: &str) -> bool {
    let base_ok = value.chars().all(|c| {
        c.is_ascii_digit()
            || matches!(c, '?' | ',' | '*' | 'H' | '/' | '-')
            || (c == 'L' && unit.allows_l())
            || (c == '#' && unit == Unit::DayOfWeek)
    }) && !value.is_empty();
    // `H(a-b)/n`, `H(a-b)` or `H/n` anywhere make the whole value acceptable.
    base_ok || hashed_forms(value)
}

fn hashed_forms(value: &str) -> bool {
    let b = value.as_bytes();
    for i in 0..b.len() {
        if b[i] != b'H' {
            continue;
        }
        let rest = &value[i + 1..];
        if let Some(r) = rest.strip_prefix('(') {
            let d1 = r.bytes().take_while(u8::is_ascii_digit).count();
            if d1 > 0 && r[d1..].starts_with('-') {
                let r2 = &r[d1 + 1..];
                let d2 = r2.bytes().take_while(u8::is_ascii_digit).count();
                if d2 > 0 && r2[d2..].starts_with(')') {
                    return true;
                }
            }
        } else if let Some(r) = rest.strip_prefix('/')
            && r.bytes().next().is_some_and(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

fn replace_aliases(value: &str) -> Result<String, String> {
    // /[a-z]{3}/gi → month or weekday number.
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 3 <= chars.len() && chars[i..i + 3].iter().all(|c| c.is_ascii_alphabetic()) {
            let word: String = chars[i..i + 3]
                .iter()
                .collect::<String>()
                .to_ascii_lowercase();
            let n = MONTHS
                .iter()
                .position(|m| *m == word)
                .map(|p| p + 1)
                .or_else(|| DAYS.iter().position(|d| *d == word))
                .ok_or_else(|| format!("Validation error, cannot resolve alias \"{word}\""))?;
            out.push_str(&n.to_string());
            i += 3;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn replace_hashed(value: &str, unit: Unit, random: f64) -> Result<String, String> {
    let (min, max) = unit.bounds();
    let b = value.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let read_num = |s: &str| -> (Option<i64>, usize) {
        let n = s.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            (None, 0)
        } else {
            (s[..n].parse().ok(), n)
        }
    };
    while i < b.len() {
        if b[i] != b'H' {
            out.push(b[i] as char);
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut range = None;
        if value[j..].starts_with('(') {
            let (a, la) = read_num(&value[j + 1..]);
            if let (Some(a), true) = (a, value[j + 1 + la..].starts_with('-')) {
                let k = j + 1 + la + 1;
                let (bb, lb) = read_num(&value[k..]);
                if let (Some(bb), true) = (bb, value[k + lb..].starts_with(')')) {
                    range = Some((a, bb));
                    j = k + lb + 1;
                }
            }
        }
        let mut step = None;
        if value[j..].starts_with('/') {
            let (s, ls) = read_num(&value[j + 1..]);
            if let Some(s) = s {
                step = Some(s);
                j += 1 + ls;
            }
        }
        let replacement = match (range, step) {
            (Some((a, bb)), Some(s)) => {
                if a > bb {
                    return Err(format!("Invalid range: {a}-{bb}, min > max"));
                }
                if s <= 0 {
                    return Err(format!("Invalid step: {s}, must be positive"));
                }
                let min_start = a.max(min);
                let offset = (random * s as f64).floor() as i64;
                let mut v = Vec::new();
                let mut x = (min_start / s) * s + offset;
                while x <= bb {
                    if x >= min_start {
                        v.push(x.to_string());
                    }
                    x += s;
                }
                v.join(",")
            }
            (Some((a, bb)), None) => {
                if a > bb {
                    return Err(format!("Invalid range: {a}-{bb}, min > max"));
                }
                ((random * (bb - a + 1) as f64).floor() as i64 + a).to_string()
            }
            (None, Some(s)) => {
                if s <= 0 {
                    return Err(format!("Invalid step: {s}, must be positive"));
                }
                let offset = (random * s as f64).floor() as i64;
                let mut v = Vec::new();
                let mut x = (min / s) * s + offset;
                while x <= max {
                    if x >= min {
                        v.push(x.to_string());
                    }
                    x += s;
                }
                v.join(",")
            }
            (None, None) => {
                ((random * (max - min + 1) as f64 + min as f64).floor() as i64).to_string()
            }
        };
        out.push_str(&replacement);
        i = j;
    }
    Ok(out)
}

fn parse_field(unit: Unit, raw: &str, random: f64) -> Result<Vec<FieldValue>, String> {
    let mut value = raw.to_string();
    if matches!(unit, Unit::Month | Unit::DayOfWeek) {
        value = replace_aliases(&value)?;
    }
    if !valid_chars(unit, &value) {
        return Err(format!("Invalid characters, got value: {value}"));
    }
    let (min, max) = unit.bounds();
    value = value.replace(['*', '?'], &format!("{min}-{max}"));
    let value = replace_hashed(&value, unit, random)?;

    let mut stack: Vec<FieldValue> = Vec::new();
    for atom in value.split(',') {
        if atom.is_empty() {
            return Err("Invalid list value format".into());
        }
        let parts: Vec<&str> = atom.split('/').collect();
        if parts.len() > 2 {
            return Err(format!("Invalid repeat: {atom}"));
        }
        let (range_src, repeat) = if parts.len() == 2 {
            let first = if js_parse_int(parts[0]).is_some() {
                format!("{}-{max}", parts[0])
            } else {
                parts[0].to_string()
            };
            (first, js_parse_int(parts[1]))
        } else {
            (atom.to_string(), Some(1))
        };
        let pieces: Vec<&str> = range_src.split('-').collect();
        if pieces.len() <= 1 {
            // A single value.
            let v = match js_number(&range_src) {
                Some(n) => FieldValue::Num(n),
                None => FieldValue::Str(range_src.clone()),
            };
            match v {
                FieldValue::Str(s) if unit.allows_l() && s.contains('L') => {
                    stack.push(FieldValue::Str(s))
                }
                other => {
                    let text = match &other {
                        FieldValue::Num(n) => n.to_string(),
                        FieldValue::Str(s) => s.clone(),
                    };
                    let n = js_parse_int(&text);
                    match n {
                        Some(n) if n >= min && n <= max => {
                            stack.push(FieldValue::Num(if unit == Unit::DayOfWeek {
                                n % 7
                            } else {
                                n
                            }))
                        }
                        _ => {
                            return Err(format!(
                                "Constraint error, got value {text} expected range {min}-{max}"
                            ));
                        }
                    }
                }
            }
            continue;
        }
        let a = js_parse_int(pieces[0]);
        let b = js_parse_int(pieces[1]);
        let (Some(a), Some(b)) = (a, b) else {
            return Err(format!(
                "Constraint error, got range {}-{} expected range {min}-{max}",
                pieces[0], pieces[1]
            ));
        };
        if a < min || b > max {
            return Err(format!(
                "Constraint error, got range {a}-{b} expected range {min}-{max}"
            ));
        }
        if a > b {
            return Err(format!("Invalid range: {a}-{b}, min({a}) > max({b})"));
        }
        let step = match repeat {
            Some(s) if s > 0 => s,
            _ => {
                return Err(format!(
                    "Constraint error, cannot repeat at every {} time.",
                    parts.get(1).unwrap_or(&"")
                ));
            }
        };
        if unit == Unit::DayOfWeek && b % 7 == 0 && !stack.contains(&FieldValue::Num(0)) {
            stack.push(FieldValue::Num(0));
        }
        let mut x = a;
        while x <= b {
            if !stack.contains(&FieldValue::Num(x)) {
                stack.push(FieldValue::Num(x));
            }
            x += step;
        }
    }
    // CronField.validate(): ranges, `L` forms, and duplicates (except a duplicate 0).
    let mut seen = std::collections::HashSet::new();
    for v in &stack {
        match v {
            FieldValue::Num(n) => {
                if *n < min || *n > max {
                    return Err(format!(
                        "Validation error, got value {n} expected range {min}-{max}"
                    ));
                }
                if !seen.insert(*n) && *n != 0 {
                    return Err(format!("Validation error, duplicate values found: {n}"));
                }
            }
            FieldValue::Str(s) => {
                let digits = s.len() - 1;
                if !(s.ends_with('L')
                    && digits <= 2
                    && s[..digits].bytes().all(|b| b.is_ascii_digit()))
                {
                    return Err(format!(
                        "Validation error, got value {s} expected range {min}-{max} or chars L"
                    ));
                }
            }
        }
    }
    if stack.is_empty() {
        return Err("Validation error, values contains no values".into());
    }
    Ok(stack)
}

/// Parse a cron expression the way cron-parser 5.5.0 does (non-strict, `hashSeed` set).
pub fn parse_cron_expression(expr: &str) -> Result<ParsedCron, String> {
    let expression = expr.trim();
    if expression.is_empty() {
        return Err("cron expression is empty".into());
    }
    if utf16_len(expression) > MAX_CRON_EXPRESSION_LENGTH {
        return Err(format!(
            "cron expression exceeds {MAX_CRON_EXPRESSION_LENGTH} characters"
        ));
    }
    let expression = match expression {
        "@yearly" | "@annually" => "0 0 0 1 1 *",
        "@monthly" => "0 0 0 1 * *",
        "@weekly" => "0 0 0 * * 0",
        "@daily" => "0 0 0 * * *",
        "@hourly" => "0 0 * * * *",
        "@minutely" => "0 * * * * *",
        "@secondly" => "* * * * * *",
        "@weekdays" => "0 0 0 * * 1-5",
        "@weekends" => "0 0 0 * * 0,6",
        other => other,
    };
    let mut atoms: Vec<String> = expression.split_whitespace().map(String::from).collect();
    if atoms.len() > 6 {
        return Err("Invalid cron expression, too many fields".into());
    }
    let defaults = ["*", "*", "*", "*", "*", "0"];
    if atoms.len() < 6 {
        let mut prefix: Vec<String> = defaults[atoms.len()..]
            .iter()
            .map(|s| s.to_string())
            .collect();
        prefix.append(&mut atoms);
        atoms = prefix;
    }
    let mut rand = Mulberry32::seeded(HASH_SEED);
    let second = parse_field(Unit::Second, &atoms[0], rand.next())?;
    let minute = parse_field(Unit::Minute, &atoms[1], rand.next())?;
    let hour = parse_field(Unit::Hour, &atoms[2], rand.next())?;
    let month = parse_field(Unit::Month, &atoms[4], rand.next())?;
    let dom = parse_field(Unit::DayOfMonth, &atoms[3], rand.next())?;
    // `#`: nth weekday of the month.
    let raw_dow = &atoms[5];
    let hash_parts: Vec<&str> = raw_dow.split('#').collect();
    let (dow_src, nth) = if hash_parts.len() <= 1 {
        (raw_dow.as_str(), 0u8)
    } else {
        if raw_dow.contains([',', '-', '/']) {
            let c = raw_dow
                .chars()
                .find(|c| matches!(c, ',' | '-' | '/'))
                .unwrap();
            return Err(format!(
                "Constraint error, invalid dayOfWeek `#` and `{c}` special characters are incompatible"
            ));
        }
        let n = js_number(hash_parts[hash_parts.len() - 1]).filter(|n| (1..=5).contains(n));
        match (hash_parts.len(), n) {
            (2, Some(n)) => (hash_parts[0], n as u8),
            _ => return Err("Constraint error, invalid dayOfWeek occurrence number (#)".into()),
        }
    };
    let dow = parse_field(Unit::DayOfWeek, dow_src, rand.next())?;

    let mut p = ParsedCron {
        seconds: [false; 60],
        minutes: [false; 60],
        hours: [false; 24],
        months: [false; 13],
        dom: [false; 32],
        dom_last: false,
        dom_wildcard: matches!(atoms[3].as_str(), "*" | "?"),
        dow: [false; 7],
        dow_last: [false; 7],
        dow_has_last: false,
        dow_last_invalid: false,
        dow_wildcard: matches!(raw_dow.as_str(), "*" | "?"),
        nth,
    };
    let nums = |vals: &[FieldValue]| {
        vals.iter()
            .filter_map(|v| {
                if let FieldValue::Num(n) = v {
                    Some(*n as usize)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    for n in nums(&second) {
        p.seconds[n] = true;
    }
    for n in nums(&minute) {
        p.minutes[n] = true;
    }
    for n in nums(&hour) {
        p.hours[n] = true;
    }
    for n in nums(&month) {
        p.months[n] = true;
    }
    for n in nums(&dom) {
        p.dom[n] = true;
    }
    p.dom_last = atoms[3].contains('L') || dom.iter().any(|v| matches!(v, FieldValue::Str(_)));
    for n in nums(&dow) {
        p.dow[n % 7] = true;
    }
    p.dow_has_last = dow_src.contains('L') || dow.iter().any(|v| matches!(v, FieldValue::Str(_)));
    if p.dow_has_last {
        // cron-parser reads the first character of every value as the weekday.
        for v in &dow {
            let first = match v {
                FieldValue::Num(n) => n.to_string().chars().next(),
                FieldValue::Str(s) => s.chars().next(),
            };
            match first.and_then(|c| c.to_digit(10)) {
                Some(d) => p.dow_last[(d % 7) as usize] = true,
                None => p.dow_last_invalid = true,
            }
        }
    }
    // "Invalid explicit day of month definition": one month and a day it doesn't have.
    let month_nums = nums(&month);
    if month_nums.len() == 1 && !p.dom_last {
        let mut dom_sorted = nums(&dom);
        dom_sorted.sort_unstable();
        if let Some(&first) = dom_sorted.first()
            && first > DAYS_IN_MONTH_MAX[month_nums[0] - 1]
        {
            return Err("Invalid explicit day of month definition".into());
        }
    }
    Ok(p)
}

const DAYS_IN_MONTH_MAX: [usize; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 => 28 + is_leap(y) as i64,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 0 = Sunday.
fn weekday(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

impl ParsedCron {
    /// cron-parser's `#matchDayOfMonth`. `Err` when it would throw (a bare `L` in day-of-week).
    fn match_day_of_month(&self, y: i64, m: i64, d: i64) -> Result<bool, ()> {
        let wd = weekday(days_from_civil(y, m, d)) as usize;
        let last = days_in_month(y, m);
        let matched_dom = self.dom[d as usize] || (self.dom_last && d == last);
        let matched_dow = self.dow[wd]
            || (self.dow_has_last && {
                if self.dow_last_invalid {
                    return Err(());
                }
                self.dow_last[wd] && d > last - 7
            });
        let restricted_dom = !self.dom_wildcard;
        let restricted_dow = !self.dow_wildcard;
        Ok(
            (restricted_dom && restricted_dow && (matched_dom || matched_dow))
                || (matched_dom && !restricted_dow)
                || (self.dom_wildcard && !self.dow_wildcard && matched_dow),
        )
    }
}

/// The smallest value in `set` greater than `current`.
fn nearest_after(set: &[bool], current: i64) -> Option<i64> {
    ((current + 1).max(0)..set.len() as i64).find(|&v| set[v as usize])
}

fn min_of(set: &[bool]) -> i64 {
    set.iter().position(|&b| b).unwrap_or(0) as i64
}

/// A port of cron-parser's `CronDate`: an instant in a zone, moved the way luxon's
/// `DateTime` arithmetic moves it (so DST behaves exactly like the TS package).
struct CronDate<'z> {
    zone: &'z Zone,
    /// Unix milliseconds.
    ts: i64,
    /// The UTC offset luxon carries with the value, in milliseconds.
    o: i64,
    dst_start: Option<i64>,
    dst_end: Option<i64>,
}

#[derive(Clone, Copy)]
struct Local {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    ms: i64,
}

fn local_ts(c: &Local) -> i64 {
    // Date.UTC, which normalizes overflowing fields.
    let (mut y, mut m) = (c.year, c.month);
    y += (m - 1).div_euclid(12);
    m = (m - 1).rem_euclid(12) + 1;
    (days_from_civil(y, m, 1) + c.day - 1) * 86_400_000
        + c.hour * 3_600_000
        + c.minute * 60_000
        + c.second * 1000
        + c.ms
}

fn days_in_month_norm(y: i64, m: i64) -> i64 {
    days_in_month(y + (m - 1).div_euclid(12), (m - 1).rem_euclid(12) + 1)
}

impl<'z> CronDate<'z> {
    fn new(zone: &'z Zone, ts: i64) -> Self {
        let o = zone.offset_at(ts.div_euclid(1000)) as i64 * 1000;
        CronDate {
            zone,
            ts,
            o,
            dst_start: None,
            dst_end: None,
        }
    }
    fn offset_ms(&self, ts: i64) -> i64 {
        self.zone.offset_at(ts.div_euclid(1000)) as i64 * 1000
    }
    /// luxon's `fixOffset`: resolve a local timestamp with an offset guess.
    fn fix_offset(&self, local: i64, o: i64) -> (i64, i64) {
        let mut utc = local - o;
        let o2 = self.offset_ms(utc);
        if o == o2 {
            return (utc, o);
        }
        utc -= o2 - o;
        let o3 = self.offset_ms(utc);
        if o2 == o3 {
            return (utc, o2);
        }
        (local - o2.min(o3), o2.max(o3))
    }
    fn local(&self) -> Local {
        let l = self.ts + self.o;
        let days = l.div_euclid(86_400_000);
        let rem = l.rem_euclid(86_400_000);
        let (year, month, day) = civil_from_days(days);
        Local {
            year,
            month,
            day,
            hour: rem / 3_600_000,
            minute: rem / 60_000 % 60,
            second: rem / 1000 % 60,
            ms: rem % 1000,
        }
    }
    fn set_local(&mut self, c: Local) {
        let (ts, o) = self.fix_offset(local_ts(&c), self.o);
        self.ts = ts;
        self.o = o;
    }
    /// luxon `plus` with calendar units (years, months, days).
    fn plus_calendar(&mut self, years: i64, months: i64, days: i64) {
        let mut c = self.local();
        c.year += years;
        c.month += months;
        c.day = c.day.min(days_in_month_norm(c.year, c.month)) + days;
        self.set_local(c);
    }
    /// luxon `plus` with time units: absolute.
    fn plus_ms(&mut self, ms: i64) {
        self.ts += ms;
        self.o = self.offset_ms(self.ts);
    }
    fn start_of_month(&mut self) {
        let mut c = self.local();
        (c.day, c.hour, c.minute, c.second, c.ms) = (1, 0, 0, 0, 0);
        self.set_local(c);
    }
    fn start_of_day(&mut self) {
        let mut c = self.local();
        (c.hour, c.minute, c.second, c.ms) = (0, 0, 0, 0);
        self.set_local(c);
    }
    fn start_of_hour(&mut self) {
        let mut c = self.local();
        (c.minute, c.second, c.ms) = (0, 0, 0);
        self.set_local(c);
    }
    fn start_of_minute(&mut self) {
        let mut c = self.local();
        (c.second, c.ms) = (0, 0);
        self.set_local(c);
    }
    fn set_hours(&mut self, h: i64) {
        let mut c = self.local();
        c.hour = h;
        self.set_local(c);
    }
    fn set_minutes(&mut self, m: i64) {
        let mut c = self.local();
        c.minute = m;
        self.set_local(c);
    }
    fn set_seconds(&mut self, s: i64) {
        let mut c = self.local();
        c.second = s;
        self.set_local(c);
    }
    fn set_milliseconds(&mut self, ms: i64) {
        let mut c = self.local();
        c.ms = ms;
        self.set_local(c);
    }
    fn add(&mut self, unit: Step) {
        match unit {
            Step::Month => {
                self.plus_calendar(0, 1, 0);
                self.start_of_month();
            }
            Step::Day => {
                self.plus_calendar(0, 0, 1);
                self.start_of_day();
            }
            Step::Hour => {
                self.plus_ms(3_600_000);
                self.start_of_hour();
            }
            Step::Minute => {
                self.plus_ms(60_000);
                self.start_of_minute();
            }
            Step::Second => self.plus_ms(1000),
        }
    }
    /// `applyDateOperation(Add, unit, hoursLength)`: notes DST jumps for hour-level steps.
    fn apply(&mut self, unit: Step, hours_length: usize) {
        if matches!(unit, Step::Month | Step::Day) {
            self.add(unit);
            return;
        }
        let previous = self.local().hour;
        self.add(unit);
        let l = self.local();
        let diff = l.hour - previous;
        if diff == 2 {
            if hours_length != 24 {
                self.dst_start = Some(l.hour);
            }
        } else if diff == 0 && l.minute == 0 && l.second == 0 && hours_length != 24 {
            self.dst_end = Some(l.hour);
        }
    }
}

#[derive(Clone, Copy)]
enum Step {
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

const LOOP_LIMIT: u32 = 10_000;

/// A port of cron-parser's `CronExpression#findSchedule` (forward). `Err` where it throws.
fn find_schedule(p: &ParsedCron, zone: &Zone, current_ms: i64) -> Result<i64, ()> {
    let mut date = CronDate::new(zone, current_ms);
    let start = date.ts;
    let hours_length = p.hours.iter().filter(|&&b| b).count();
    let mut dst_cache: Option<((i64, i64, i64), bool)> = None;
    let mut step_count = 0;
    loop {
        step_count += 1;
        if step_count >= LOOP_LIMIT {
            // cron-parser exits here without throwing and returns wherever it stopped.
            break;
        }
        let l = date.local();
        if !p.match_day_of_month(l.year, l.month, l.day)? {
            date.apply(Step::Day, hours_length);
            continue;
        }
        if !(p.nth == 0 || (l.day + 6) / 7 == p.nth as i64) {
            date.apply(Step::Day, hours_length);
            continue;
        }
        if !p.months[l.month as usize] {
            date.apply(Step::Month, hours_length);
            continue;
        }
        // #matchHour
        let current_hour = l.hour;
        let is_match = p.hours[current_hour as usize];
        if date.dst_start == Some(current_hour) {
            if current_hour >= 1 && p.hours[(current_hour - 1) as usize] {
                // matched via the skipped hour
            } else {
                date.add(Step::Hour);
                continue;
            }
        } else if date.dst_end == Some(current_hour) {
            date.dst_end = None;
            date.apply(Step::Hour, hours_length);
            continue;
        } else if !is_match {
            date.dst_start = None;
            match nearest_after(&p.hours, current_hour) {
                None => {
                    date.apply(Step::Day, hours_length);
                }
                Some(next_hour) => {
                    // On DST transition days, step hour by hour so the DST flags get set.
                    let key = (l.year, l.month, l.day);
                    let transition_day = match dst_cache {
                        Some((k, v)) if k == key => v,
                        _ => {
                            let mut a = CronDate::new(zone, date.ts);
                            a.o = date.o;
                            a.start_of_day();
                            // luxon endOf('day'): plus one day, start of day, minus 1 ms.
                            let mut b = CronDate::new(zone, date.ts);
                            b.o = date.o;
                            b.plus_calendar(0, 0, 1);
                            b.start_of_day();
                            let end = b.ts - 1;
                            let v = a.o != date.offset_ms(end);
                            dst_cache = Some((key, v));
                            v
                        }
                    };
                    if transition_day {
                        for _ in 0..(next_hour - current_hour) {
                            date.apply(Step::Hour, hours_length);
                        }
                    } else {
                        date.set_hours(next_hour);
                    }
                    date.set_minutes(min_of(&p.minutes));
                    date.set_seconds(min_of(&p.seconds));
                }
            }
            continue;
        }
        let l = date.local();
        if !p.minutes[l.minute as usize] {
            match nearest_after(&p.minutes, l.minute) {
                Some(m) => {
                    date.set_minutes(m);
                    date.set_seconds(min_of(&p.seconds));
                }
                None => {
                    date.apply(Step::Hour, hours_length);
                    date.set_minutes(min_of(&p.minutes));
                    date.set_seconds(min_of(&p.seconds));
                }
            }
            continue;
        }
        if !p.seconds[l.second as usize] {
            match nearest_after(&p.seconds, l.second) {
                Some(s) => date.set_seconds(s),
                None => {
                    date.apply(Step::Minute, hours_length);
                    date.set_seconds(min_of(&p.seconds));
                }
            }
            continue;
        }
        if start == date.ts {
            date.apply(Step::Second, hours_length);
            continue;
        }
        break;
    }
    if date.local().ms != 0 {
        date.set_milliseconds(0);
    }
    Ok(date.ts)
}

/// The first occurrence strictly after `after_micros` in `timezone`, at whole-second
/// precision, exactly as the TS package computes it with cron-parser (DST quirks included).
/// `None` when there is none, or the zone is invalid.
pub fn next_fire_after(parsed: &ParsedCron, after_micros: i64, timezone: &str) -> Option<i64> {
    let zone = zone(timezone)?;
    let after_ms = after_micros / 1000;
    if after_ms.abs() > MAX_SAFE_DATE_MS {
        return None;
    }
    let next = find_schedule(parsed, &zone, after_ms).ok()?;
    if next.abs() > MAX_SAFE_DATE_MS {
        return None;
    }
    Some(next * 1000)
}

// ---------------------------------------------------------------------------
// Time zones
// ---------------------------------------------------------------------------

/// Whether `tz` names a zone (case-insensitive IANA name, `UTC`, or a `±HH:MM` offset).
pub fn is_valid_timezone(tz: &str) -> bool {
    !tz.is_empty() && utf16_len(tz) <= MAX_TIMEZONE_LENGTH && tz == tz.trim() && zone(tz).is_some()
}

/// The UTC offset in seconds at `unix_micros` in `tz`, or `None` for an unknown zone.
pub fn utc_offset_seconds(tz: &str, unix_micros: i64) -> Option<i32> {
    Some(zone(tz)?.offset_at(unix_micros.div_euclid(ONE_SECOND_MICROS)))
}

#[derive(Clone, Copy, Debug)]
struct PosixRule {
    std: i32,
    dst: Option<(i32, RuleDate, i32, RuleDate, i32)>,
}

#[derive(Clone, Copy, Debug)]
enum RuleDate {
    /// Mm.w.d
    Month(i64, i64, i64),
    /// Jn: day 1-365, Feb 29 never counted
    Julian(i64),
    /// n: day 0-365, leap days counted
    Zero(i64),
}

enum Zone {
    Fixed(i32),
    Rule(PosixRule),
    Explicit {
        initial: i32,
        transitions: &'static [(i64, i32)],
        rule: PosixRule,
    },
}

fn zone(tz: &str) -> Option<Zone> {
    if tz.is_empty() || utf16_len(tz) > MAX_TIMEZONE_LENGTH || tz != tz.trim() {
        return None;
    }
    if let Some(off) = parse_offset_zone(tz) {
        return Some(Zone::Fixed(off));
    }
    let lower = tz.to_ascii_lowercase();
    let idx = TZ_NAMES
        .binary_search_by(|(n, _)| (*n).cmp(lower.as_str()))
        .ok()?;
    let rule = parse_posix(TZ_RULES[TZ_NAMES[idx].1 as usize])?;
    if let Ok(i) = TZ_EXPLICIT.binary_search_by(|(n, _, _)| (*n).cmp(lower.as_str())) {
        let (_, initial, transitions) = TZ_EXPLICIT[i];
        return Some(Zone::Explicit {
            initial,
            transitions,
            rule,
        });
    }
    Some(match rule.dst {
        None => Zone::Fixed(rule.std),
        Some(_) => Zone::Rule(rule),
    })
}

/// `+HH:MM`, `-HH:MM`, `+HHMM` or `+HH` (like ECMAScript offset time zones).
fn parse_offset_zone(tz: &str) -> Option<i32> {
    let b = tz.as_bytes();
    let sign = match b.first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: Vec<u8> = b[1..].iter().copied().filter(|c| *c != b':').collect();
    let colon_ok = match b.len() {
        3 => true,                    // +HH
        5 => !b[1..].contains(&b':'), // +HHMM
        6 => b[3] == b':',            // +HH:MM
        _ => false,
    };
    if !colon_ok || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let h = ((digits[0] - b'0') * 10 + (digits[1] - b'0')) as i32;
    let m = if digits.len() == 4 {
        ((digits[2] - b'0') * 10 + (digits[3] - b'0')) as i32
    } else {
        0
    };
    if h > 23 || m > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

fn parse_posix(s: &str) -> Option<PosixRule> {
    let b = s.as_bytes();
    let mut i = 0;
    fn name(b: &[u8], i: &mut usize) -> Option<()> {
        if b.get(*i) == Some(&b'<') {
            *i += b[*i..].iter().position(|c| *c == b'>')? + 1;
        } else {
            let n = b[*i..]
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .count();
            if n < 3 {
                return None;
            }
            *i += n;
        }
        Some(())
    }
    fn time(b: &[u8], i: &mut usize) -> Option<i32> {
        let mut sign = 1;
        if let Some(c @ (b'+' | b'-')) = b.get(*i) {
            sign = if *c == b'-' { -1 } else { 1 };
            *i += 1;
        }
        let mut parts = [0i32; 3];
        for (k, part) in parts.iter_mut().enumerate() {
            if k > 0 {
                if b.get(*i) != Some(&b':') {
                    break;
                }
                *i += 1;
            }
            let n = b[*i..].iter().take_while(|c| c.is_ascii_digit()).count();
            if n == 0 {
                return None;
            }
            *part = std::str::from_utf8(&b[*i..*i + n]).ok()?.parse().ok()?;
            *i += n;
        }
        Some(sign * (parts[0] * 3600 + parts[1] * 60 + parts[2]))
    }
    fn date(b: &[u8], i: &mut usize) -> Option<(RuleDate, i32)> {
        let num = |b: &[u8], i: &mut usize| -> Option<i64> {
            let n = b[*i..].iter().take_while(|c| c.is_ascii_digit()).count();
            let v = std::str::from_utf8(&b[*i..*i + n]).ok()?.parse().ok()?;
            *i += n;
            Some(v)
        };
        let d = match b.get(*i)? {
            b'M' => {
                *i += 1;
                let m = num(b, i)?;
                *i += 1;
                let w = num(b, i)?;
                *i += 1;
                let d = num(b, i)?;
                RuleDate::Month(m, w, d)
            }
            b'J' => {
                *i += 1;
                RuleDate::Julian(num(b, i)?)
            }
            _ => RuleDate::Zero(num(b, i)?),
        };
        let mut t = 7200;
        if b.get(*i) == Some(&b'/') {
            *i += 1;
            t = time(b, i)?;
        }
        Some((d, t))
    }
    name(b, &mut i)?;
    let std = -time(b, &mut i)?;
    if i == b.len() {
        return Some(PosixRule { std, dst: None });
    }
    name(b, &mut i)?;
    let mut dst = std + 3600;
    if i < b.len() && b[i] != b',' {
        dst = -time(b, &mut i)?;
    }
    let (start, start_t, end, end_t) = if b.get(i) == Some(&b',') {
        i += 1;
        let (s, st) = date(b, &mut i)?;
        if b.get(i) != Some(&b',') {
            return None;
        }
        i += 1;
        let (e, et) = date(b, &mut i)?;
        (s, st, e, et)
    } else {
        (
            RuleDate::Month(3, 2, 0),
            7200,
            RuleDate::Month(11, 1, 0),
            7200,
        )
    };
    Some(PosixRule {
        std,
        dst: Some((dst, start, start_t, end, end_t)),
    })
}

fn rule_day(date: RuleDate, y: i64) -> i64 {
    match date {
        RuleDate::Julian(n) => days_from_civil(y, 1, 1) + n - 1 + (is_leap(y) && n > 59) as i64,
        RuleDate::Zero(n) => days_from_civil(y, 1, 1) + n,
        RuleDate::Month(m, w, d) => {
            let first = days_from_civil(y, m, 1);
            let mut day = first + (d - weekday(first)).rem_euclid(7) + (w - 1) * 7;
            while day >= first + days_in_month(y, m) {
                day -= 7;
            }
            day
        }
    }
}

impl PosixRule {
    fn offset_at(&self, t: i64) -> i32 {
        let Some((dst, start, start_t, end, end_t)) = self.dst else {
            return self.std;
        };
        let y = civil_from_days(t.div_euclid(86400)).0;
        let mut events: Vec<(i64, i32)> = Vec::with_capacity(6);
        for yy in [y - 1, y, y + 1] {
            events.push((
                rule_day(start, yy) * 86400 + start_t as i64 - self.std as i64,
                dst,
            ));
            events.push((
                rule_day(end, yy) * 86400 + end_t as i64 - dst as i64,
                self.std,
            ));
        }
        events.sort_unstable();
        let mut cur = if events[0].1 == dst { self.std } else { dst };
        for (when, off) in events {
            if when <= t {
                cur = off;
            }
        }
        cur
    }
}

impl Zone {
    fn offset_at(&self, t: i64) -> i32 {
        match self {
            Zone::Fixed(o) => *o,
            Zone::Rule(r) => r.offset_at(t),
            Zone::Explicit {
                initial,
                transitions,
                rule,
            } => match transitions.last() {
                Some((last, _)) if t >= *last => rule.offset_at(t),
                _ => match transitions.partition_point(|(at, _)| *at <= t) {
                    0 => *initial,
                    k => transitions[k - 1].1,
                },
            },
        }
    }
}

// BEGIN GENERATED TZ (tools/gen-tz.py, tzdata 2026c)
/// IANA tzdata version the zone table was generated from.
pub const TZDATA_VERSION: &str = "2026c";
#[rustfmt::skip]
const TZ_RULES: &[&str] = &[
    "<+00>0", "<+00>0<+02>-2,M3.5.0/1,M10.5.0/3", "<+01>-1", "<+02>-2", "<+0330>-3:30", "<+03>-3",
    "<+0430>-4:30", "<+04>-4", "<+0530>-5:30", "<+0545>-5:45", "<+05>-5", "<+0630>-6:30", "<+06>-6",
    "<+07>-7", "<+0845>-8:45", "<+08>-8", "<+09>-9", "<+1030>-10:30<+11>-11,M10.1.0,M4.1.0", "<+10>-10",
    "<+11>-11", "<+11>-11<+12>,M10.1.0,M4.1.0/3", "<+1245>-12:45<+1345>,M9.5.0/2:45,M4.1.0/3:45", "<+12>-12",
    "<+13>-13", "<+14>-14", "<-01>1", "<-01>1<+00>,M3.5.0/0,M10.5.0/1", "<-02>2",
    "<-02>2<-01>,M3.5.0/-1,M10.5.0/0", "<-03>3", "<-03>3<-02>,M3.2.0,M11.1.0", "<-04>4",
    "<-04>4<-03>,M9.1.6/24,M4.1.6/24", "<-05>5", "<-06>6", "<-06>6<-05>,M9.1.6/22,M4.1.6/22", "<-07>7",
    "<-08>8", "<-0930>9:30", "<-09>9", "<-10>10", "<-11>11", "<-12>12", "ACST-9:30",
    "ACST-9:30ACDT,M10.1.0,M4.1.0/3", "AEST-10", "AEST-10AEDT,M10.1.0,M4.1.0/3", "AKST9AKDT,M3.2.0,M11.1.0",
    "AST4", "AST4ADT,M3.2.0,M11.1.0", "AWST-8", "CAT-2", "CET-1", "CET-1CEST,M3.5.0,M10.5.0/3", "CST-8",
    "CST5CDT,M3.2.0/0,M11.1.0/1", "CST6", "CST6CDT,M3.2.0,M11.1.0", "ChST-10", "EAT-3", "EET-2",
    "EET-2EEST,M3.4.4/50,M10.4.4/50", "EET-2EEST,M3.5.0/0,M10.5.0/0", "EET-2EEST,M3.5.0/3,M10.5.0/4",
    "EET-2EEST,M4.5.5/0,M10.5.4/24", "EST5", "EST5EDT,M3.2.0,M11.1.0", "GMT0", "GMT0BST,M3.5.0/1,M10.5.0",
    "GMT0IST,M3.5.0/1,M10.5.0", "HKT-8", "HST10", "HST10HDT,M3.2.0,M11.1.0", "IST-2IDT,M3.4.4/26,M10.5.0",
    "IST-5:30", "JST-9", "KST-9", "MET-1MEST,M3.5.0,M10.5.0/3", "MSK-3", "MST7", "MST7MDT,M3.2.0,M11.1.0",
    "NST3:30NDT,M3.2.0,M11.1.0", "NZST-12NZDT,M9.5.0,M4.1.0/3", "PKT-5", "PST-8", "PST8PDT,M3.2.0,M11.1.0",
    "SAST-2", "SST11", "UTC0", "WAT-1", "WET0WEST,M3.5.0/1,M10.5.0", "WIB-7", "WIT-9", "WITA-8",
];
/// Lowercase zone names (sorted) and their index into `TZ_RULES`.
#[rustfmt::skip]
const TZ_NAMES: &[(&str, u16)] = &[
    ("africa/abidjan", 67), ("africa/accra", 67), ("africa/addis_ababa", 59), ("africa/algiers", 52),
    ("africa/asmara", 59), ("africa/asmera", 59), ("africa/bamako", 67), ("africa/bangui", 89),
    ("africa/banjul", 67), ("africa/bissau", 67), ("africa/blantyre", 51), ("africa/brazzaville", 89),
    ("africa/bujumbura", 51), ("africa/cairo", 64), ("africa/casablanca", 0), ("africa/ceuta", 53),
    ("africa/conakry", 67), ("africa/dakar", 67), ("africa/dar_es_salaam", 59), ("africa/djibouti", 59),
    ("africa/douala", 89), ("africa/el_aaiun", 0), ("africa/freetown", 67), ("africa/gaborone", 51),
    ("africa/harare", 51), ("africa/johannesburg", 86), ("africa/juba", 51), ("africa/kampala", 59),
    ("africa/khartoum", 51), ("africa/kigali", 51), ("africa/kinshasa", 89), ("africa/lagos", 89),
    ("africa/libreville", 89), ("africa/lome", 67), ("africa/luanda", 89), ("africa/lubumbashi", 51),
    ("africa/lusaka", 51), ("africa/malabo", 89), ("africa/maputo", 51), ("africa/maseru", 86),
    ("africa/mbabane", 86), ("africa/mogadishu", 59), ("africa/monrovia", 67), ("africa/nairobi", 59),
    ("africa/ndjamena", 89), ("africa/niamey", 89), ("africa/nouakchott", 67), ("africa/ouagadougou", 67),
    ("africa/porto-novo", 89), ("africa/sao_tome", 67), ("africa/timbuktu", 67), ("africa/tripoli", 60),
    ("africa/tunis", 52), ("africa/windhoek", 51), ("america/adak", 72), ("america/anchorage", 47),
    ("america/anguilla", 48), ("america/antigua", 48), ("america/araguaina", 29),
    ("america/argentina/buenos_aires", 29), ("america/argentina/catamarca", 29),
    ("america/argentina/comodrivadavia", 29), ("america/argentina/cordoba", 29),
    ("america/argentina/jujuy", 29), ("america/argentina/la_rioja", 29), ("america/argentina/mendoza", 29),
    ("america/argentina/rio_gallegos", 29), ("america/argentina/salta", 29),
    ("america/argentina/san_juan", 29), ("america/argentina/san_luis", 29),
    ("america/argentina/tucuman", 29), ("america/argentina/ushuaia", 29), ("america/aruba", 48),
    ("america/asuncion", 29), ("america/atikokan", 65), ("america/atka", 72), ("america/bahia", 29),
    ("america/bahia_banderas", 56), ("america/barbados", 48), ("america/belem", 29), ("america/belize", 56),
    ("america/blanc-sablon", 48), ("america/boa_vista", 31), ("america/bogota", 33), ("america/boise", 80),
    ("america/buenos_aires", 29), ("america/cambridge_bay", 80), ("america/campo_grande", 31),
    ("america/cancun", 65), ("america/caracas", 31), ("america/catamarca", 29), ("america/cayenne", 29),
    ("america/cayman", 65), ("america/chicago", 57), ("america/chihuahua", 56),
    ("america/ciudad_juarez", 80), ("america/coral_harbour", 65), ("america/cordoba", 29),
    ("america/costa_rica", 56), ("america/coyhaique", 29), ("america/creston", 79), ("america/cuiaba", 31),
    ("america/curacao", 48), ("america/danmarkshavn", 67), ("america/dawson", 79),
    ("america/dawson_creek", 79), ("america/denver", 80), ("america/detroit", 66), ("america/dominica", 48),
    ("america/edmonton", 56), ("america/eirunepe", 33), ("america/el_salvador", 56),
    ("america/ensenada", 85), ("america/fort_nelson", 79), ("america/fort_wayne", 66),
    ("america/fortaleza", 29), ("america/glace_bay", 49), ("america/godthab", 28), ("america/goose_bay", 49),
    ("america/grand_turk", 66), ("america/grenada", 48), ("america/guadeloupe", 48),
    ("america/guatemala", 56), ("america/guayaquil", 33), ("america/guyana", 31), ("america/halifax", 49),
    ("america/havana", 55), ("america/hermosillo", 79), ("america/indiana/indianapolis", 66),
    ("america/indiana/knox", 57), ("america/indiana/marengo", 66), ("america/indiana/petersburg", 66),
    ("america/indiana/tell_city", 57), ("america/indiana/vevay", 66), ("america/indiana/vincennes", 66),
    ("america/indiana/winamac", 66), ("america/indianapolis", 66), ("america/inuvik", 80),
    ("america/iqaluit", 66), ("america/jamaica", 65), ("america/jujuy", 29), ("america/juneau", 47),
    ("america/kentucky/louisville", 66), ("america/kentucky/monticello", 66), ("america/knox_in", 57),
    ("america/kralendijk", 48), ("america/la_paz", 31), ("america/lima", 33), ("america/los_angeles", 85),
    ("america/louisville", 66), ("america/lower_princes", 48), ("america/maceio", 29),
    ("america/managua", 56), ("america/manaus", 31), ("america/marigot", 48), ("america/martinique", 48),
    ("america/matamoros", 57), ("america/mazatlan", 79), ("america/mendoza", 29), ("america/menominee", 57),
    ("america/merida", 56), ("america/metlakatla", 47), ("america/mexico_city", 56),
    ("america/miquelon", 30), ("america/moncton", 49), ("america/monterrey", 56), ("america/montevideo", 29),
    ("america/montreal", 66), ("america/montserrat", 48), ("america/nassau", 66), ("america/new_york", 66),
    ("america/nipigon", 66), ("america/nome", 47), ("america/noronha", 27),
    ("america/north_dakota/beulah", 57), ("america/north_dakota/center", 57),
    ("america/north_dakota/new_salem", 57), ("america/nuuk", 28), ("america/ojinaga", 57),
    ("america/panama", 65), ("america/pangnirtung", 66), ("america/paramaribo", 29), ("america/phoenix", 79),
    ("america/port-au-prince", 66), ("america/port_of_spain", 48), ("america/porto_acre", 33),
    ("america/porto_velho", 31), ("america/puerto_rico", 48), ("america/punta_arenas", 29),
    ("america/rainy_river", 57), ("america/rankin_inlet", 57), ("america/recife", 29),
    ("america/regina", 56), ("america/resolute", 57), ("america/rio_branco", 33), ("america/rosario", 29),
    ("america/santa_isabel", 85), ("america/santarem", 29), ("america/santiago", 32),
    ("america/santo_domingo", 48), ("america/sao_paulo", 29), ("america/scoresbysund", 28),
    ("america/shiprock", 80), ("america/sitka", 47), ("america/st_barthelemy", 48), ("america/st_johns", 81),
    ("america/st_kitts", 48), ("america/st_lucia", 48), ("america/st_thomas", 48),
    ("america/st_vincent", 48), ("america/swift_current", 56), ("america/tegucigalpa", 56),
    ("america/thule", 49), ("america/thunder_bay", 66), ("america/tijuana", 85), ("america/toronto", 66),
    ("america/tortola", 48), ("america/vancouver", 79), ("america/virgin", 48), ("america/whitehorse", 79),
    ("america/winnipeg", 57), ("america/yakutat", 47), ("america/yellowknife", 56), ("antarctica/casey", 15),
    ("antarctica/davis", 13), ("antarctica/dumontdurville", 18), ("antarctica/macquarie", 46),
    ("antarctica/mawson", 10), ("antarctica/mcmurdo", 82), ("antarctica/palmer", 29),
    ("antarctica/rothera", 29), ("antarctica/south_pole", 82), ("antarctica/syowa", 5),
    ("antarctica/troll", 1), ("antarctica/vostok", 10), ("arctic/longyearbyen", 53), ("asia/aden", 5),
    ("asia/almaty", 10), ("asia/amman", 5), ("asia/anadyr", 22), ("asia/aqtau", 10), ("asia/aqtobe", 10),
    ("asia/ashgabat", 10), ("asia/ashkhabad", 10), ("asia/atyrau", 10), ("asia/baghdad", 5),
    ("asia/bahrain", 5), ("asia/baku", 7), ("asia/bangkok", 13), ("asia/barnaul", 13), ("asia/beirut", 62),
    ("asia/bishkek", 12), ("asia/brunei", 15), ("asia/calcutta", 74), ("asia/chita", 16),
    ("asia/choibalsan", 15), ("asia/chongqing", 54), ("asia/chungking", 54), ("asia/colombo", 8),
    ("asia/dacca", 12), ("asia/damascus", 5), ("asia/dhaka", 12), ("asia/dili", 16), ("asia/dubai", 7),
    ("asia/dushanbe", 10), ("asia/famagusta", 63), ("asia/gaza", 61), ("asia/harbin", 54),
    ("asia/hebron", 61), ("asia/ho_chi_minh", 13), ("asia/hong_kong", 70), ("asia/hovd", 13),
    ("asia/irkutsk", 15), ("asia/istanbul", 5), ("asia/jakarta", 91), ("asia/jayapura", 92),
    ("asia/jerusalem", 73), ("asia/kabul", 6), ("asia/kamchatka", 22), ("asia/karachi", 83),
    ("asia/kashgar", 12), ("asia/kathmandu", 9), ("asia/katmandu", 9), ("asia/khandyga", 16),
    ("asia/kolkata", 74), ("asia/krasnoyarsk", 13), ("asia/kuala_lumpur", 15), ("asia/kuching", 15),
    ("asia/kuwait", 5), ("asia/macao", 54), ("asia/macau", 54), ("asia/magadan", 19), ("asia/makassar", 93),
    ("asia/manila", 84), ("asia/muscat", 7), ("asia/nicosia", 63), ("asia/novokuznetsk", 13),
    ("asia/novosibirsk", 13), ("asia/omsk", 12), ("asia/oral", 10), ("asia/phnom_penh", 13),
    ("asia/pontianak", 91), ("asia/pyongyang", 76), ("asia/qatar", 5), ("asia/qostanay", 10),
    ("asia/qyzylorda", 10), ("asia/rangoon", 11), ("asia/riyadh", 5), ("asia/saigon", 13),
    ("asia/sakhalin", 19), ("asia/samarkand", 10), ("asia/seoul", 76), ("asia/shanghai", 54),
    ("asia/singapore", 15), ("asia/srednekolymsk", 19), ("asia/taipei", 54), ("asia/tashkent", 10),
    ("asia/tbilisi", 7), ("asia/tehran", 4), ("asia/tel_aviv", 73), ("asia/thimbu", 12),
    ("asia/thimphu", 12), ("asia/tokyo", 75), ("asia/tomsk", 13), ("asia/ujung_pandang", 93),
    ("asia/ulaanbaatar", 15), ("asia/ulan_bator", 15), ("asia/urumqi", 12), ("asia/ust-nera", 18),
    ("asia/vientiane", 13), ("asia/vladivostok", 18), ("asia/yakutsk", 16), ("asia/yangon", 11),
    ("asia/yekaterinburg", 10), ("asia/yerevan", 7), ("atlantic/azores", 26), ("atlantic/bermuda", 49),
    ("atlantic/canary", 90), ("atlantic/cape_verde", 25), ("atlantic/faeroe", 90), ("atlantic/faroe", 90),
    ("atlantic/jan_mayen", 53), ("atlantic/madeira", 90), ("atlantic/reykjavik", 67),
    ("atlantic/south_georgia", 27), ("atlantic/st_helena", 67), ("atlantic/stanley", 29),
    ("australia/act", 46), ("australia/adelaide", 44), ("australia/brisbane", 45),
    ("australia/broken_hill", 44), ("australia/canberra", 46), ("australia/currie", 46),
    ("australia/darwin", 43), ("australia/eucla", 14), ("australia/hobart", 46), ("australia/lhi", 17),
    ("australia/lindeman", 45), ("australia/lord_howe", 17), ("australia/melbourne", 46),
    ("australia/north", 43), ("australia/nsw", 46), ("australia/perth", 50), ("australia/queensland", 45),
    ("australia/south", 44), ("australia/sydney", 46), ("australia/tasmania", 46),
    ("australia/victoria", 46), ("australia/west", 50), ("australia/yancowinna", 44), ("brazil/acre", 33),
    ("brazil/denoronha", 27), ("brazil/east", 29), ("brazil/west", 31), ("canada/atlantic", 49),
    ("canada/central", 57), ("canada/eastern", 66), ("canada/mountain", 56), ("canada/newfoundland", 81),
    ("canada/pacific", 79), ("canada/saskatchewan", 56), ("canada/yukon", 79), ("cet", 53),
    ("chile/continental", 32), ("chile/easterisland", 35), ("cst6cdt", 57), ("cuba", 55), ("eet", 63),
    ("egypt", 64), ("eire", 69), ("est", 65), ("est5edt", 66), ("etc/gmt", 67), ("etc/gmt+0", 67),
    ("etc/gmt+1", 25), ("etc/gmt+10", 40), ("etc/gmt+11", 41), ("etc/gmt+12", 42), ("etc/gmt+2", 27),
    ("etc/gmt+3", 29), ("etc/gmt+4", 31), ("etc/gmt+5", 33), ("etc/gmt+6", 34), ("etc/gmt+7", 36),
    ("etc/gmt+8", 37), ("etc/gmt+9", 39), ("etc/gmt-0", 67), ("etc/gmt-1", 2), ("etc/gmt-10", 18),
    ("etc/gmt-11", 19), ("etc/gmt-12", 22), ("etc/gmt-13", 23), ("etc/gmt-14", 24), ("etc/gmt-2", 3),
    ("etc/gmt-3", 5), ("etc/gmt-4", 7), ("etc/gmt-5", 10), ("etc/gmt-6", 12), ("etc/gmt-7", 13),
    ("etc/gmt-8", 15), ("etc/gmt-9", 16), ("etc/gmt0", 67), ("etc/greenwich", 67), ("etc/uct", 88),
    ("etc/universal", 88), ("etc/utc", 88), ("etc/zulu", 88), ("europe/amsterdam", 53),
    ("europe/andorra", 53), ("europe/astrakhan", 7), ("europe/athens", 63), ("europe/belfast", 68),
    ("europe/belgrade", 53), ("europe/berlin", 53), ("europe/bratislava", 53), ("europe/brussels", 53),
    ("europe/bucharest", 63), ("europe/budapest", 53), ("europe/busingen", 53), ("europe/chisinau", 63),
    ("europe/copenhagen", 53), ("europe/dublin", 69), ("europe/gibraltar", 53), ("europe/guernsey", 68),
    ("europe/helsinki", 63), ("europe/isle_of_man", 68), ("europe/istanbul", 5), ("europe/jersey", 68),
    ("europe/kaliningrad", 60), ("europe/kiev", 63), ("europe/kirov", 78), ("europe/kyiv", 63),
    ("europe/lisbon", 90), ("europe/ljubljana", 53), ("europe/london", 68), ("europe/luxembourg", 53),
    ("europe/madrid", 53), ("europe/malta", 53), ("europe/mariehamn", 63), ("europe/minsk", 5),
    ("europe/monaco", 53), ("europe/moscow", 78), ("europe/nicosia", 63), ("europe/oslo", 53),
    ("europe/paris", 53), ("europe/podgorica", 53), ("europe/prague", 53), ("europe/riga", 63),
    ("europe/rome", 53), ("europe/samara", 7), ("europe/san_marino", 53), ("europe/sarajevo", 53),
    ("europe/saratov", 7), ("europe/simferopol", 78), ("europe/skopje", 53), ("europe/sofia", 63),
    ("europe/stockholm", 53), ("europe/tallinn", 63), ("europe/tirane", 53), ("europe/tiraspol", 63),
    ("europe/ulyanovsk", 7), ("europe/uzhgorod", 63), ("europe/vaduz", 53), ("europe/vatican", 53),
    ("europe/vienna", 53), ("europe/vilnius", 63), ("europe/volgograd", 78), ("europe/warsaw", 53),
    ("europe/zagreb", 53), ("europe/zaporozhye", 63), ("europe/zurich", 53), ("gb", 68), ("gb-eire", 68),
    ("gmt", 67), ("gmt+0", 67), ("gmt-0", 67), ("gmt0", 67), ("greenwich", 67), ("hongkong", 70),
    ("hst", 71), ("iceland", 67), ("indian/antananarivo", 59), ("indian/chagos", 12),
    ("indian/christmas", 13), ("indian/cocos", 11), ("indian/comoro", 59), ("indian/kerguelen", 10),
    ("indian/mahe", 7), ("indian/maldives", 10), ("indian/mauritius", 7), ("indian/mayotte", 59),
    ("indian/reunion", 7), ("iran", 4), ("israel", 73), ("jamaica", 65), ("japan", 75), ("kwajalein", 22),
    ("libya", 60), ("met", 77), ("mexico/bajanorte", 85), ("mexico/bajasur", 79), ("mexico/general", 56),
    ("mst", 79), ("mst7mdt", 80), ("navajo", 80), ("nz", 82), ("nz-chat", 21), ("pacific/apia", 23),
    ("pacific/auckland", 82), ("pacific/bougainville", 19), ("pacific/chatham", 21), ("pacific/chuuk", 18),
    ("pacific/easter", 35), ("pacific/efate", 19), ("pacific/enderbury", 23), ("pacific/fakaofo", 23),
    ("pacific/fiji", 22), ("pacific/funafuti", 22), ("pacific/galapagos", 34), ("pacific/gambier", 39),
    ("pacific/guadalcanal", 19), ("pacific/guam", 58), ("pacific/honolulu", 71), ("pacific/johnston", 71),
    ("pacific/kanton", 23), ("pacific/kiritimati", 24), ("pacific/kosrae", 19), ("pacific/kwajalein", 22),
    ("pacific/majuro", 22), ("pacific/marquesas", 38), ("pacific/midway", 87), ("pacific/nauru", 22),
    ("pacific/niue", 41), ("pacific/norfolk", 20), ("pacific/noumea", 19), ("pacific/pago_pago", 87),
    ("pacific/palau", 16), ("pacific/pitcairn", 37), ("pacific/pohnpei", 19), ("pacific/ponape", 19),
    ("pacific/port_moresby", 18), ("pacific/rarotonga", 40), ("pacific/saipan", 58), ("pacific/samoa", 87),
    ("pacific/tahiti", 40), ("pacific/tarawa", 22), ("pacific/tongatapu", 23), ("pacific/truk", 18),
    ("pacific/wake", 22), ("pacific/wallis", 22), ("pacific/yap", 18), ("poland", 53), ("portugal", 90),
    ("prc", 54), ("pst8pdt", 85), ("roc", 54), ("rok", 76), ("singapore", 15), ("turkey", 5), ("uct", 88),
    ("universal", 88), ("us/alaska", 47), ("us/aleutian", 72), ("us/arizona", 79), ("us/central", 57),
    ("us/east-indiana", 66), ("us/eastern", 66), ("us/hawaii", 71), ("us/indiana-starke", 57),
    ("us/michigan", 66), ("us/mountain", 80), ("us/pacific", 85), ("us/samoa", 87), ("utc", 88),
    ("w-su", 78), ("wet", 90), ("zulu", 88),
];
/// Zones whose tzdata transitions from 2025 on aren't expressible by their rule:
/// (name, UTC offset at 2025-01-01, [(transition unix seconds, UTC offset after)]).
/// The rule in `TZ_RULES` applies after the last transition.
type TzExplicit = (&'static str, i32, &'static [(i64, i32)]);
#[rustfmt::skip]
const TZ_EXPLICIT: &[TzExplicit] = &[
    ("africa/casablanca", 3600, &[
        (1740276000, 0), (1743904800, 3600), (1771120800, 0), (1774144800, 3600), (1789866000, 0),
        (2147483647, 0),
    ]),
    ("africa/el_aaiun", 3600, &[
        (1740276000, 0), (1743904800, 3600), (1771120800, 0), (1774144800, 3600), (1789866000, 0),
        (2147483647, 0),
    ]),
    ("america/edmonton", -25200, &[
        (1741510800, -21600), (1762070400, -25200), (1772960400, -21600), (1793520000, -21600),
    ]),
    ("america/vancouver", -28800, &[
        (1741514400, -25200), (1762074000, -28800), (1772964000, -25200), (1793523600, -25200),
    ]),
    ("america/yellowknife", -25200, &[
        (1741510800, -21600), (1762070400, -25200), (1772960400, -21600), (1793520000, -21600),
    ]),
    ("asia/gaza", 7200, &[
        (1744416000, 10800), (1761346800, 7200), (1774656000, 10800), (1792796400, 7200),
        (1806105600, 10800), (1824850800, 7200), (1837555200, 10800), (1856300400, 7200),
        (1869004800, 10800), (1887750000, 7200), (1901059200, 10800), (1919199600, 7200),
        (1932508800, 10800), (1950649200, 7200), (1963958400, 10800), (1982703600, 7200),
        (1995408000, 10800), (2014153200, 7200), (2026857600, 10800), (2045602800, 7200),
        (2058307200, 10800), (2077052400, 7200), (2090361600, 10800), (2107897200, 7200),
        (2121811200, 10800), (2138742000, 7200), (2153260800, 10800), (2168982000, 7200),
        (2184710400, 10800), (2199826800, 7200), (2216160000, 10800), (2230066800, 7200),
        (2234304000, 10800), (2234905200, 7200), (2248214400, 10800), (2260911600, 7200),
        (2264544000, 10800), (2266354800, 7200), (2279664000, 10800), (2291756400, 7200),
        (2295388800, 10800), (2297804400, 7200), (2311113600, 10800), (2321996400, 7200),
        (2326233600, 10800), (2329254000, 7200), (2342563200, 10800), (2352841200, 7200),
        (2356473600, 10800), (2361308400, 7200), (2374012800, 10800), (2383686000, 7200),
        (2387318400, 10800), (2392758000, 7200), (2405462400, 10800), (2413926000, 7200),
        (2418163200, 10800), (2424207600, 7200), (2437516800, 10800), (2444770800, 7200),
        (2448403200, 10800), (2455657200, 7200), (2468966400, 10800), (2475010800, 7200),
        (2479248000, 10800), (2487106800, 7200), (2500416000, 10800), (2505855600, 7200),
        (2509488000, 10800), (2519161200, 7200), (2531865600, 10800), (2536700400, 7200),
        (2540332800, 10800), (2550610800, 7200), (2563315200, 10800), (2566940400, 7200),
        (2571177600, 10800), (2582060400, 7200), (2595369600, 10800), (2597785200, 7200),
        (2601417600, 10800), (2613510000, 7200), (2626819200, 10800), (2628025200, 7200),
        (2632262400, 10800), (2644959600, 7200), (2658268800, 10800), (2658870000, 7200),
        (2663107200, 10800), (2676409200, 7200), (2693347200, 10800), (2708463600, 7200),
        (2724192000, 10800), (2739913200, 7200), (2754432000, 10800), (2771362800, 7200),
        (2785276800, 10800), (2802812400, 7200), (2816121600, 10800), (2834262000, 7200),
        (2847571200, 10800), (2866316400, 7200), (2879020800, 10800), (2897766000, 7200),
        (2910470400, 10800), (2929215600, 7200), (2941920000, 10800), (2960665200, 7200),
        (2973974400, 10800), (2992114800, 7200), (3005424000, 10800), (3023564400, 7200),
        (3036873600, 10800), (3055618800, 7200), (3068323200, 10800), (3087068400, 7200),
        (3099772800, 10800), (3117913200, 7200), (3131827200, 10800), (3148758000, 7200),
        (3163276800, 10800), (3179602800, 7200), (3194726400, 10800), (3209842800, 7200),
        (3226176000, 10800), (3240687600, 7200), (3244320000, 10800), (3244921200, 7200),
        (3257625600, 10800), (3271532400, 7200), (3275164800, 10800), (3276370800, 7200),
        (3289075200, 10800), (3301772400, 7200), (3306009600, 10800), (3307820400, 7200),
        (3321129600, 10800), (3332617200, 7200), (3336249600, 10800), (3339270000, 7200),
        (3352579200, 10800), (3362857200, 7200), (3367094400, 10800), (3370719600, 7200),
        (3384028800, 10800), (3393702000, 7200), (3397939200, 10800), (3402774000, 7200),
        (3415478400, 10800), (3424546800, 7200), (3428179200, 10800), (3434223600, 7200),
        (3446928000, 10800), (3454786800, 7200), (3459024000, 10800), (3465673200, 7200),
        (3478982400, 10800), (3485631600, 7200), (3489264000, 10800), (3497122800, 7200),
        (3510432000, 10800), (3516476400, 7200), (3520108800, 10800), (3528572400, 7200),
        (3541881600, 10800), (3546716400, 7200), (3550953600, 10800), (3560022000, 7200),
        (3573331200, 10800), (3577561200, 7200), (3581193600, 10800), (3592076400, 7200),
        (3604780800, 10800), (3607801200, 7200), (3612038400, 10800), (3623526000, 7200),
        (3636230400, 10800), (3638646000, 7200), (3642883200, 10800), (3654975600, 7200),
        (3668284800, 10800), (3669490800, 7200), (3673123200, 10800), (3686425200, 7200),
    ]),
    ("asia/hebron", 7200, &[
        (1744416000, 10800), (1761346800, 7200), (1774656000, 10800), (1792796400, 7200),
        (1806105600, 10800), (1824850800, 7200), (1837555200, 10800), (1856300400, 7200),
        (1869004800, 10800), (1887750000, 7200), (1901059200, 10800), (1919199600, 7200),
        (1932508800, 10800), (1950649200, 7200), (1963958400, 10800), (1982703600, 7200),
        (1995408000, 10800), (2014153200, 7200), (2026857600, 10800), (2045602800, 7200),
        (2058307200, 10800), (2077052400, 7200), (2090361600, 10800), (2107897200, 7200),
        (2121811200, 10800), (2138742000, 7200), (2153260800, 10800), (2168982000, 7200),
        (2184710400, 10800), (2199826800, 7200), (2216160000, 10800), (2230066800, 7200),
        (2234304000, 10800), (2234905200, 7200), (2248214400, 10800), (2260911600, 7200),
        (2264544000, 10800), (2266354800, 7200), (2279664000, 10800), (2291756400, 7200),
        (2295388800, 10800), (2297804400, 7200), (2311113600, 10800), (2321996400, 7200),
        (2326233600, 10800), (2329254000, 7200), (2342563200, 10800), (2352841200, 7200),
        (2356473600, 10800), (2361308400, 7200), (2374012800, 10800), (2383686000, 7200),
        (2387318400, 10800), (2392758000, 7200), (2405462400, 10800), (2413926000, 7200),
        (2418163200, 10800), (2424207600, 7200), (2437516800, 10800), (2444770800, 7200),
        (2448403200, 10800), (2455657200, 7200), (2468966400, 10800), (2475010800, 7200),
        (2479248000, 10800), (2487106800, 7200), (2500416000, 10800), (2505855600, 7200),
        (2509488000, 10800), (2519161200, 7200), (2531865600, 10800), (2536700400, 7200),
        (2540332800, 10800), (2550610800, 7200), (2563315200, 10800), (2566940400, 7200),
        (2571177600, 10800), (2582060400, 7200), (2595369600, 10800), (2597785200, 7200),
        (2601417600, 10800), (2613510000, 7200), (2626819200, 10800), (2628025200, 7200),
        (2632262400, 10800), (2644959600, 7200), (2658268800, 10800), (2658870000, 7200),
        (2663107200, 10800), (2676409200, 7200), (2693347200, 10800), (2708463600, 7200),
        (2724192000, 10800), (2739913200, 7200), (2754432000, 10800), (2771362800, 7200),
        (2785276800, 10800), (2802812400, 7200), (2816121600, 10800), (2834262000, 7200),
        (2847571200, 10800), (2866316400, 7200), (2879020800, 10800), (2897766000, 7200),
        (2910470400, 10800), (2929215600, 7200), (2941920000, 10800), (2960665200, 7200),
        (2973974400, 10800), (2992114800, 7200), (3005424000, 10800), (3023564400, 7200),
        (3036873600, 10800), (3055618800, 7200), (3068323200, 10800), (3087068400, 7200),
        (3099772800, 10800), (3117913200, 7200), (3131827200, 10800), (3148758000, 7200),
        (3163276800, 10800), (3179602800, 7200), (3194726400, 10800), (3209842800, 7200),
        (3226176000, 10800), (3240687600, 7200), (3244320000, 10800), (3244921200, 7200),
        (3257625600, 10800), (3271532400, 7200), (3275164800, 10800), (3276370800, 7200),
        (3289075200, 10800), (3301772400, 7200), (3306009600, 10800), (3307820400, 7200),
        (3321129600, 10800), (3332617200, 7200), (3336249600, 10800), (3339270000, 7200),
        (3352579200, 10800), (3362857200, 7200), (3367094400, 10800), (3370719600, 7200),
        (3384028800, 10800), (3393702000, 7200), (3397939200, 10800), (3402774000, 7200),
        (3415478400, 10800), (3424546800, 7200), (3428179200, 10800), (3434223600, 7200),
        (3446928000, 10800), (3454786800, 7200), (3459024000, 10800), (3465673200, 7200),
        (3478982400, 10800), (3485631600, 7200), (3489264000, 10800), (3497122800, 7200),
        (3510432000, 10800), (3516476400, 7200), (3520108800, 10800), (3528572400, 7200),
        (3541881600, 10800), (3546716400, 7200), (3550953600, 10800), (3560022000, 7200),
        (3573331200, 10800), (3577561200, 7200), (3581193600, 10800), (3592076400, 7200),
        (3604780800, 10800), (3607801200, 7200), (3612038400, 10800), (3623526000, 7200),
        (3636230400, 10800), (3638646000, 7200), (3642883200, 10800), (3654975600, 7200),
        (3668284800, 10800), (3669490800, 7200), (3673123200, 10800), (3686425200, 7200),
    ]),
    ("canada/mountain", -25200, &[
        (1741510800, -21600), (1762070400, -25200), (1772960400, -21600), (1793520000, -21600),
    ]),
    ("canada/pacific", -28800, &[
        (1741514400, -25200), (1762074000, -28800), (1772964000, -25200), (1793523600, -25200),
    ]),
];
// END GENERATED TZ
