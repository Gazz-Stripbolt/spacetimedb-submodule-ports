//! Demo for lobby.rs: the upstream example module (install + the client operations), the
//! ranked host operations from the README, and procedures the test suite uses to act as any
//! subject and to run the matchmaking helpers.

#[path = "../../../rust/lobby.rs"]
pub mod lobby;

use lobby::*;
use spacetimedb::{ProcedureContext, ReducerContext, SpacetimeType, Timestamp};

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    lobby::install(ctx);
}

#[spacetimedb::procedure]
pub fn demo_join_queue(
    ctx: &mut ProcedureContext,
    pool: String,
    subject: String,
    match_size: u32,
    attributes_json: Option<String>,
    ttl_seconds: Option<u32>,
) -> Result<LobbyJoinQueueResult, String> {
    ctx.try_with_tx(|tx| {
        lobby::join_queue_for_subject(
            tx,
            &JoinQueueArgs {
                pool: pool.clone(),
                subject: subject.clone(),
                match_size,
                attributes_json: attributes_json.clone(),
                ttl_seconds,
            },
        )
    })
}

#[spacetimedb::procedure]
#[allow(clippy::too_many_arguments)]
pub fn demo_join_ranked_queue(
    ctx: &mut ProcedureContext,
    pool: String,
    subject: String,
    match_size: u32,
    attributes_json: Option<String>,
    ttl_seconds: Option<u32>,
    rating_pool: Option<String>,
) -> Result<LobbyJoinQueueResult, String> {
    ctx.try_with_tx(|tx| {
        lobby::join_ranked_queue_for_subject(
            tx,
            &JoinRankedQueueArgs {
                pool: pool.clone(),
                subject: subject.clone(),
                match_size,
                attributes_json: attributes_json.clone(),
                ttl_seconds,
                rating_pool: rating_pool.clone(),
            },
        )
    })
}

#[spacetimedb::reducer]
pub fn demo_cancel_ticket(
    ctx: &ReducerContext,
    ticket_id: String,
    subject: String,
) -> Result<(), String> {
    lobby::cancel_ticket_for_subject(ctx, &ticket_id, &subject)
}

#[spacetimedb::reducer]
pub fn demo_join_room(ctx: &ReducerContext, room_id: u64, subject: String) -> Result<(), String> {
    lobby::join_room_for_subject(ctx, room_id, &subject)
}

#[spacetimedb::reducer]
pub fn demo_leave_room(ctx: &ReducerContext, room_id: u64, subject: String) -> Result<(), String> {
    lobby::leave_room_for_subject(ctx, room_id, &subject)
}

#[spacetimedb::reducer]
pub fn demo_report_result(
    ctx: &ReducerContext,
    room_id: u64,
    winner_subject: Option<String>,
) -> Result<(), String> {
    lobby::report_match_result(ctx, room_id, winner_subject.as_deref())
}

#[spacetimedb::reducer]
pub fn demo_close_room(ctx: &ReducerContext, room_id: u64) -> Result<(), String> {
    lobby::close_room(ctx, room_id)
}

/// Run one sweep pass now (the scheduled one runs every 15 s).
#[spacetimedb::reducer]
pub fn demo_sweep(ctx: &ReducerContext) -> Result<(), String> {
    lobby::require_admin(ctx)?;
    lobby::sweep(ctx)
}

#[derive(SpacetimeType)]
pub struct DemoTicket {
    pub ticket_id: String,
    pub rating: Option<i32>,
    pub rating_pool: Option<String>,
    pub created_at_micros: i64,
}

/// `rankedSelection` over synthetic tickets; returns the picked ticket ids.
#[spacetimedb::procedure]
pub fn demo_ranked_selection(
    _ctx: &mut ProcedureContext,
    tickets: Vec<DemoTicket>,
    match_size: u32,
    now_micros: i64,
) -> Option<Vec<String>> {
    let queued: Vec<LobbyQueueTicket> = tickets
        .iter()
        .map(|t| LobbyQueueTicket {
            ticket_id: t.ticket_id.clone(),
            pool: "p".into(),
            subject: t.ticket_id.clone(),
            status: LobbyTicketStatus::Queued,
            match_size,
            ranked: true,
            rating: t.rating,
            rating_pool: t.rating_pool.clone(),
            party_id: None,
            attributes_json: None,
            room_id: None,
            created_at: Timestamp::from_micros_since_unix_epoch(t.created_at_micros),
            updated_at: Timestamp::from_micros_since_unix_epoch(t.created_at_micros),
            expires_at: Timestamp::from_micros_since_unix_epoch(i64::MAX),
        })
        .collect();
    lobby::ranked_selection(&queued, match_size as usize, now_micros).map(|picked| {
        picked
            .iter()
            .map(|&i| queued[i].ticket_id.clone())
            .collect()
    })
}

#[spacetimedb::procedure]
pub fn demo_elo(_ctx: &mut ProcedureContext, rating: i32, opponent: i32, score: f64) -> Vec<f64> {
    let e = lobby::expected_score(rating, opponent);
    vec![e, lobby::updated_rating(rating, e, score) as f64]
}

#[spacetimedb::procedure]
pub fn demo_band(_ctx: &mut ProcedureContext, created_at_micros: i64, now_micros: i64) -> i64 {
    lobby::ranked_band(
        Timestamp::from_micros_since_unix_epoch(created_at_micros),
        now_micros,
    )
}

#[spacetimedb::procedure]
pub fn demo_key(_ctx: &mut ProcedureContext, parts: Vec<String>) -> String {
    lobby::lobby_composite_key(&parts.iter().map(String::as_str).collect::<Vec<_>>())
}
