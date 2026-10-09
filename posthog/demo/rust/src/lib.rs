//! Demo for posthog.rs: the standalone package's procedures, reducers and views, plus the
//! README's host pattern: an application reducer that records an analytics event in the same
//! transaction as its own change. Test hooks pause the scheduler and run retention at a given time.

#[path = "../../../rust/posthog.rs"]
pub mod posthog;

use posthog::posthog_flush_tick as _;
use spacetimedb::{ReducerContext, Table, Timestamp};

#[spacetimedb::table(accessor = demo_order)]
pub struct DemoOrder {
    #[primary_key]
    pub order_id: String,
    pub total_cents: u64,
}

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    posthog::install(ctx);
}

/// Any user: completes an order and queues `order_completed` in the same transaction. Bad
/// analytics input never rolls the order back (the event is stored as Rejected instead).
#[spacetimedb::reducer]
pub fn complete_order(
    ctx: &ReducerContext,
    order_id: String,
    total_cents: u64,
    properties_override: Option<String>,
) {
    ctx.db.demo_order().insert(DemoOrder {
        order_id: order_id.clone(),
        total_cents,
    });
    posthog::enqueue_event_in_tx(
        ctx,
        posthog::EnqueueEventArgs {
            distinct_id: ctx.sender().to_hex().to_string(),
            event: "order_completed".into(),
            properties_json: Some(properties_override.unwrap_or_else(|| {
                format!(r#"{{"orderId":"{order_id}","totalCents":"{total_cents}"}}"#)
            })),
            idempotency_key: Some(format!("order_completed:{order_id}")),
        },
    );
}

fn require_admin(ctx: &ReducerContext) -> Result<(), String> {
    if posthog::is_admin(ctx, ctx.sender()) {
        Ok(())
    } else {
        Err(posthog::errors::NOT_AUTHORIZED.into())
    }
}

/// Test hook: stop the 5-second flush so tests can drive delivery by hand.
#[spacetimedb::reducer]
pub fn demo_pause_scheduler(ctx: &ReducerContext) -> Result<(), String> {
    require_admin(ctx)?;
    let ids: Vec<u64> = ctx
        .db
        .posthog_flush_tick()
        .iter()
        .map(|t| t.scheduled_id)
        .collect();
    for id in ids {
        ctx.db.posthog_flush_tick().scheduled_id().delete(id);
    }
    Ok(())
}

/// Test hook: restart it (what `install` does).
#[spacetimedb::reducer]
pub fn demo_resume_scheduler(ctx: &ReducerContext) -> Result<(), String> {
    require_admin(ctx)?;
    posthog::install(ctx);
    Ok(())
}

/// Test hook: run the retention sweep as of `now_micros`.
#[spacetimedb::reducer]
pub fn demo_prune_history_at(ctx: &ReducerContext, now_micros: i64) -> Result<(), String> {
    require_admin(ctx)?;
    posthog::prune_delivery_history(ctx, Timestamp::from_micros_since_unix_epoch(now_micros));
    Ok(())
}
