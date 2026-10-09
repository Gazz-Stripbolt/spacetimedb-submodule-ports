//! Demo for rate_limit.rs: the upstream example (install + admin reducers) plus procedures the
//! test suite uses to consume and peek with arbitrary policies.

#[path = "../../../rust/rate_limit.rs"]
pub mod rate_limit;

use rate_limit::RateLimitPolicy;
use spacetimedb::{ProcedureContext, ReducerContext, SpacetimeType, Timestamp};

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    rate_limit::install(ctx);
}

/// A [`rate_limit::RateLimitResult`] as a SpacetimeDB type, so a procedure can return it.
#[derive(SpacetimeType)]
pub struct DemoConsumeResult {
    pub allowed: bool,
    pub key: String,
    pub scope: String,
    pub limit: u32,
    pub used: u32,
    pub remaining: u32,
    pub reset_at: Timestamp,
    pub retry_after_seconds: u32,
}

#[derive(SpacetimeType)]
pub struct DemoPeekResult {
    pub limit: u32,
    pub window_seconds: u32,
    pub used: u32,
    pub remaining: u32,
    pub reset_at: Option<Timestamp>,
}

/// Policies are `const` in real code; the demo builds them from arguments. Leaking the scope
/// string is fine for a demo (each scope leaks once per instance).
fn demo_policy(scope: String, limit: u32, window_seconds: u32) -> RateLimitPolicy {
    rate_limit::policy(Box::leak(scope.into_boxed_str()), limit, window_seconds)
}

#[spacetimedb::procedure]
pub fn demo_consume(
    ctx: &mut ProcedureContext,
    scope: String,
    limit: u32,
    window_seconds: u32,
    key: String,
    cost: u32,
) -> Result<DemoConsumeResult, String> {
    let p = demo_policy(scope, limit, window_seconds);
    ctx.try_with_tx(|tx| {
        let r = p.consume(tx, &key, cost)?;
        Ok(DemoConsumeResult {
            allowed: r.allowed,
            key: r.key,
            scope: r.scope,
            limit: r.limit,
            used: r.used,
            remaining: r.remaining,
            reset_at: r.reset_at,
            retry_after_seconds: r.retry_after_seconds,
        })
    })
}

/// Peek, with `now` from the procedure's clock when `use_now` is set.
#[spacetimedb::procedure]
pub fn demo_peek(
    ctx: &mut ProcedureContext,
    scope: String,
    limit: u32,
    window_seconds: u32,
    key: String,
    use_now: bool,
) -> Result<DemoPeekResult, String> {
    let p = demo_policy(scope, limit, window_seconds);
    ctx.try_with_tx(|tx| {
        let s = p.peek(tx, &key, use_now.then_some(tx.timestamp))?;
        Ok(DemoPeekResult {
            limit: s.limit,
            window_seconds: s.window_seconds,
            used: s.used,
            remaining: s.remaining,
            reset_at: s.reset_at,
        })
    })
}
