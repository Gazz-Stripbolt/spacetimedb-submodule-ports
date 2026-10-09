//! Demo for presence.rs: the upstream example module (install + the client operations) plus
//! procedures the test suite uses to drive the host helpers in arbitrary scopes.

#[path = "../../../rust/presence.rs"]
pub mod presence;

use presence::{PresenceEntry, PresenceUpsertOpts};
use spacetimedb::{ProcedureContext, ReducerContext};

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    presence::install(ctx);
}

#[spacetimedb::procedure]
pub fn demo_upsert(
    ctx: &mut ProcedureContext,
    scope: String,
    subject: String,
    status: Option<String>,
    activity: Option<String>,
    payload_json: Option<String>,
    ttl_seconds: Option<u32>,
) -> Result<PresenceEntry, String> {
    ctx.try_with_tx(|tx| {
        presence::upsert_presence(
            tx,
            &PresenceUpsertOpts {
                scope: scope.clone(),
                subject: subject.clone(),
                status: status.clone(),
                activity: activity.clone(),
                payload_json: payload_json.clone(),
                ttl_seconds,
            },
        )
    })
}

#[spacetimedb::procedure]
pub fn demo_touch(
    ctx: &mut ProcedureContext,
    scope: String,
    subject: String,
    ttl_seconds: Option<u32>,
) -> Result<PresenceEntry, String> {
    ctx.try_with_tx(|tx| presence::touch_presence(tx, &scope, &subject, ttl_seconds))
}

#[spacetimedb::procedure]
pub fn demo_remove(
    ctx: &mut ProcedureContext,
    scope: String,
    subject: String,
) -> Result<bool, String> {
    ctx.try_with_tx(|tx| presence::remove_presence(tx, &scope, &subject))
}

#[spacetimedb::procedure]
pub fn demo_key(
    _ctx: &mut ProcedureContext,
    scope: String,
    subject: String,
) -> Result<String, String> {
    presence::build_presence_key(&scope, &subject)
}
