//! Demo for retry.rs: the upstream example's `flaky` task plus a few tasks the test suite uses.

#[path = "../../../rust/retry.rs"]
pub mod retry;

use retry::retry_task as _;
use spacetimedb::{ReducerContext, ScheduleAt, SpacetimeType, Table, Timestamp};

#[derive(SpacetimeType, Clone)]
pub struct FlakyArgs {
    pub task_name: String,
    pub succeed_at_attempt: u8,
    pub throw_on_failure: bool,
}

/// One variant per task, named like the TS task tags.
#[derive(SpacetimeType, Clone)]
#[allow(non_camel_case_types)]
pub enum RetryArgs {
    flaky(FlakyArgs),
    fails,
    rejects,
    long_error,
    succeeds,
}

#[spacetimedb::table(accessor = retry_metric, public)]
pub struct RetryMetric {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub name: String,
    pub value: f64,
    pub recorded_at: Timestamp,
}

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    retry::install(ctx);
}

/// The one handler retry.rs calls, dispatching on the task's arguments.
pub fn retry_handle(ctx: &ReducerContext, args: &RetryArgs) -> retry::RetryResult {
    match args {
        RetryArgs::flaky(a) => {
            // The attempt being run is still in `retry_task` while its reducer runs.
            let attempt = ctx
                .db
                .retry_task()
                .name()
                .filter(&a.task_name)
                .next()
                .map_or(0, |t| t.attempt);
            if attempt < a.succeed_at_attempt {
                // Rust can't throw (a panic would abort the whole attempt), so
                // `throw_on_failure` takes the same path as a returned failure.
                return retry::failed(format!("simulated failure at attempt {attempt}"));
            }
            ctx.db.retry_metric().insert(RetryMetric {
                id: 0,
                name: format!("flaky-success-{}", a.task_name),
                value: attempt as f64,
                recorded_at: ctx.timestamp,
            });
            retry::ok()
        }
        RetryArgs::fails => retry::failed("unavailable"),
        RetryArgs::rejects => retry::permanent("order.not_found"),
        RetryArgs::long_error => retry::failed("x".repeat(3000)),
        RetryArgs::succeeds => retry::ok(),
    }
}

/// Test helper: run `count` successful attempts inline (exercises the 1,000-row history cap).
#[spacetimedb::reducer]
pub fn demo_run_inline(ctx: &ReducerContext, count: u32) -> Result<(), String> {
    retry::require_admin(ctx)?;
    let task = retry::RetryTask {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Time(ctx.timestamp),
        name: "inline".into(),
        args: RetryArgs::succeeds,
        attempt: 0,
        max_attempts: 1,
        backoff_secs: 1,
    };
    for _ in 0..count {
        retry::run_attempt(ctx, &task);
    }
    Ok(())
}
