//! Demo for cron.rs: the upstream demo module's jobs and management reducers, plus procedures
//! that expose the expression parser and time-zone table to the test suite.

#[path = "../../../rust/cron.rs"]
pub mod cron;

use cron::{CronInvocation, ScheduleOpts};
use spacetimedb::{ProcedureContext, ReducerContext, SpacetimeType, Table, Timestamp, ViewContext};

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct ReportCronArgs {
    pub label: String,
    pub batch_size: u32,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct ProbeCronArgs {
    pub source: String,
}

cron_jobs! {
    history_cap: 5,
    reconcile_every_seconds: 2,
    jobs: {
        heartbeat => reducer,
        report(ReportCronArgs) => reducer,
        flaky => reducer,
        probe(ProbeCronArgs) => procedure,
    }
}

#[spacetimedb::table(accessor = tick_log, public)]
pub struct TickLog {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub job_name: String,
    pub at: Timestamp,
}

#[spacetimedb::table(accessor = flaky_state, public)]
pub struct FlakyState {
    #[primary_key]
    pub singleton: bool,
    pub failing: bool,
}

#[spacetimedb::table(accessor = argument_log, public)]
pub struct ArgumentLog {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub job_name: String,
    pub value: String,
    pub count: u32,
    pub invocation_id: String,
}

/// Reducer jobs.
pub fn cron_run(
    ctx: &ReducerContext,
    job: &CronJobArgsValue,
    inv: &CronInvocation,
) -> Result<(), String> {
    match job {
        CronJobArgsValue::heartbeat => {
            ctx.db.tick_log().insert(TickLog {
                id: 0,
                job_name: "heartbeat".into(),
                at: ctx.timestamp,
            });
        }
        CronJobArgsValue::report(args) => {
            ctx.db.tick_log().insert(TickLog {
                id: 0,
                job_name: "report".into(),
                at: ctx.timestamp,
            });
            ctx.db.argument_log().insert(ArgumentLog {
                id: 0,
                job_name: "report".into(),
                value: args.label.clone(),
                count: args.batch_size,
                invocation_id: inv.id.clone(),
            });
        }
        CronJobArgsValue::flaky => {
            // Written, then rolled back if the job fails.
            ctx.db.tick_log().insert(TickLog {
                id: 0,
                job_name: "flaky".into(),
                at: ctx.timestamp,
            });
            if ctx
                .db
                .flaky_state()
                .singleton()
                .find(true)
                .is_some_and(|s| s.failing)
            {
                return Err("flaky.failure".into());
            }
        }
        CronJobArgsValue::probe(_) => unreachable!("probe is a procedure job"),
    }
    Ok(())
}

/// Procedure jobs: the natural home for side effects such as outbound HTTP.
pub fn cron_run_procedure(
    ctx: &mut ProcedureContext,
    job: &CronJobArgsValue,
    inv: &CronInvocation,
) -> Result<(), String> {
    let CronJobArgsValue::probe(args) = job else {
        unreachable!()
    };
    let failing = ctx.with_tx(|tx| {
        tx.db
            .flaky_state()
            .singleton()
            .find(true)
            .is_some_and(|s| s.failing)
    });
    if failing {
        return Err("probe.failure".into());
    }
    ctx.with_tx(|tx| {
        tx.db.tick_log().insert(TickLog {
            id: 0,
            job_name: "probe".into(),
            at: tx.timestamp,
        });
        tx.db.argument_log().insert(ArgumentLog {
            id: 0,
            job_name: "probe".into(),
            value: args.source.clone(),
            count: 0,
            invocation_id: inv.id.clone(),
        });
    });
    Ok(())
}

#[spacetimedb::view(accessor = cron_jobs, public)]
fn cron_jobs(ctx: &ViewContext) -> Vec<cron::CronJobView> {
    cron::public_jobs(ctx)
}

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) -> Result<(), String> {
    ctx.db.flaky_state().insert(FlakyState {
        singleton: true,
        failing: false,
    });
    cron::schedule(
        ctx,
        CronJobArgsValue::heartbeat,
        cron::every(30),
        ScheduleOpts::default(),
    )
}

fn argumentless(name: &str) -> Result<CronJobArgsValue, String> {
    match name {
        "heartbeat" => Ok(CronJobArgsValue::heartbeat),
        "flaky" => Ok(CronJobArgsValue::flaky),
        _ => Err(format!("test.unknown_job:{name}")),
    }
}

fn opts(timezone: String, max_failures: u32) -> ScheduleOpts {
    ScheduleOpts {
        timezone: Some(timezone),
        max_failures: Some(max_failures),
    }
}

#[spacetimedb::reducer]
pub fn schedule_cron(
    ctx: &ReducerContext,
    name: String,
    expression: String,
    timezone: String,
    max_failures: u32,
) -> Result<(), String> {
    cron::schedule(
        ctx,
        argumentless(&name)?,
        cron::cron(expression),
        opts(timezone, max_failures),
    )
}

#[spacetimedb::reducer]
pub fn schedule_every(
    ctx: &ReducerContext,
    name: String,
    seconds: u32,
    max_failures: u32,
) -> Result<(), String> {
    cron::schedule(
        ctx,
        argumentless(&name)?,
        cron::every(seconds),
        ScheduleOpts {
            timezone: None,
            max_failures: Some(max_failures),
        },
    )
}

#[spacetimedb::reducer]
pub fn schedule_report(
    ctx: &ReducerContext,
    expression: String,
    timezone: String,
    max_failures: u32,
    label: String,
    batch_size: u32,
) -> Result<(), String> {
    let args = CronJobArgsValue::report(ReportCronArgs { label, batch_size });
    cron::schedule(
        ctx,
        args,
        cron::cron(expression),
        opts(timezone, max_failures),
    )
}

#[spacetimedb::reducer]
pub fn schedule_probe(
    ctx: &ReducerContext,
    expression: String,
    timezone: String,
    max_failures: u32,
    source: String,
) -> Result<(), String> {
    let args = CronJobArgsValue::probe(ProbeCronArgs { source });
    cron::schedule(
        ctx,
        args,
        cron::cron(expression),
        opts(timezone, max_failures),
    )
}

#[spacetimedb::reducer]
pub fn unschedule_job(ctx: &ReducerContext, name: String) -> Result<(), String> {
    cron::unschedule(ctx, &name)
}

#[spacetimedb::reducer]
pub fn set_flaky_failing(ctx: &ReducerContext, failing: bool) {
    if let Some(s) = ctx.db.flaky_state().singleton().find(true) {
        ctx.db
            .flaky_state()
            .singleton()
            .update(FlakyState { failing, ..s });
    }
}

/// Fault injection for the lost-fire reconciler.
#[spacetimedb::reducer]
pub fn drop_heartbeat_fire_for_test(ctx: &ReducerContext) {
    use cron::cron_fire as _;
    ctx.db
        .cron_fire()
        .job_name()
        .delete("heartbeat".to_string());
}

/// Authorization probe: a direct call must not run a scheduler-only reducer.
#[spacetimedb::reducer]
pub fn forge_report_fire_for_test(ctx: &ReducerContext) -> Result<(), String> {
    use cron::cron_fire as _;
    let fire = ctx
        .db
        .cron_fire()
        .job_name()
        .find("report".to_string())
        .ok_or("test.missing_report_fire")?;
    cron::cron_tick(ctx, fire)
}

// Parser and time-zone probes for the shared test vectors.

/// Next occurrence after `after_micros`, or the parse error.
#[spacetimedb::procedure]
pub fn cron_next_fire_after(
    _ctx: &mut ProcedureContext,
    expression: String,
    after_micros: i64,
    timezone: String,
) -> Result<Option<i64>, String> {
    let parsed = cron::parse_cron_expression(&expression)?;
    Ok(cron::next_fire_after(&parsed, after_micros, &timezone))
}

#[spacetimedb::procedure]
pub fn cron_is_valid_timezone(_ctx: &mut ProcedureContext, timezone: String) -> bool {
    cron::is_valid_timezone(&timezone)
}

#[spacetimedb::procedure]
pub fn cron_utc_offset(
    _ctx: &mut ProcedureContext,
    timezone: String,
    at_micros: i64,
) -> Option<i32> {
    cron::utc_offset_seconds(&timezone, at_micros)
}

#[derive(SpacetimeType)]
pub struct CronCase {
    pub expression: String,
    pub after_micros: i64,
    pub timezone: String,
}

/// Batch form of `cron_next_fire_after` for the vector suite. Parse errors answer `None`.
#[spacetimedb::procedure]
pub fn cron_next_batch(_ctx: &mut ProcedureContext, cases: Vec<CronCase>) -> Vec<Option<i64>> {
    cases
        .iter()
        .map(|c| {
            cron::parse_cron_expression(&c.expression)
                .ok()
                .and_then(|p| cron::next_fire_after(&p, c.after_micros, &c.timezone))
        })
        .collect()
}

#[spacetimedb::procedure]
pub fn cron_parse_batch(_ctx: &mut ProcedureContext, expressions: Vec<String>) -> Vec<bool> {
    expressions
        .iter()
        .map(|e| cron::parse_cron_expression(e).is_ok())
        .collect()
}

#[spacetimedb::procedure]
pub fn cron_valid_timezone_batch(_ctx: &mut ProcedureContext, timezones: Vec<String>) -> Vec<bool> {
    timezones
        .iter()
        .map(|t| cron::is_valid_timezone(t))
        .collect()
}

#[derive(SpacetimeType)]
pub struct TzCase {
    pub timezone: String,
    pub unix_seconds: i64,
}

#[spacetimedb::procedure]
pub fn cron_utc_offset_batch(_ctx: &mut ProcedureContext, cases: Vec<TzCase>) -> Vec<Option<i32>> {
    cases
        .iter()
        .map(|c| cron::utc_offset_seconds(&c.timezone, c.unix_seconds * 1_000_000))
        .collect()
}
