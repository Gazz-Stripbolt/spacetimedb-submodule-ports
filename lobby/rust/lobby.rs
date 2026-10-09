//! # lobby.rs: matchmaking queues, rooms, seats and Elo ratings, for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/lobby` (SpacetimeDB PR #5908, commit
//! `f6ed7dcd1c8f853c97fc7e9f126606fca7adadac`, package `spacetime-lobby-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Same private tables (`lobby_config`, `lobby_admin_identity`, `lobby_queue_ticket`,
//! `lobby_subject_rating`, `lobby_room`, `lobby_match_result`, `lobby_room_seat`,
//! `lobby_sweep_tick`), indexes, limits, matchmaking (unranked first-come and ranked bands
//! that widen with waiting time), Elo (K = 32), lifecycle rules, error codes and views as the
//! TS package.
//!
//! ```ignore
//! #[spacetimedb::reducer(init)]
//! pub fn init(ctx: &ReducerContext) { lobby::install(ctx); }
//!
//! // Ranked queues and results are host-only (rating pools are public on the leaderboard):
//! #[spacetimedb::reducer]
//! pub fn queue_ranked(ctx: &ReducerContext) -> Result<(), String> {
//!     lobby::join_ranked_queue_for_subject(ctx, &lobby::JoinRankedQueueArgs {
//!         pool: "duel".into(), subject: ctx.sender().to_hex().to_string(), match_size: 2, ..Default::default()
//!     }).map(|_| ())
//! }
//! ```
//!
//! Changes from the TypeScript original:
//! - The generic client reducer names are prefixed: `join_queue` → `lobby_join_queue`,
//!   `cancel_ticket` → `lobby_cancel_ticket`, `join_room` → `lobby_join_room`, `leave_room` →
//!   `lobby_leave_room`, `set_rating` → `lobby_set_rating`, `update_config` →
//!   `lobby_update_config` (TS mounts them under a submodule namespace).
//! - `get_lobby_status` returns `Result<LobbyStatus, String>`.
//! - `lobby_sweep` rejects callers other than the scheduler.
//! - Pools sort by plain string order in the views (TS uses `localeCompare`, an ICU collation
//!   modules don't have). The same for ASCII pool names that don't mix cases.
//! - `lobby_admin_room_seats` walks the `room_id` index (Rust views can't scan tables), so
//!   its rows come out grouped by room.

use spacetimedb::{
    Identity, ProcedureContext, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration,
    Timestamp, ViewContext,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

pub const DEFAULT_RATING: i32 = 1000;
pub const MIN_RATING: i32 = 100;
pub const MAX_RATING: i32 = 5000;
const ELO_K: f64 = 32.0;
const RANKED_INITIAL_BAND: i64 = 100;
const RANKED_BAND_STEP: i64 = 50;
const RANKED_BAND_STEP_SECONDS: i64 = 10;
const RANKED_MAX_BAND: i64 = 800;

const MAX_POOL_LENGTH: usize = 96;
const MAX_SUBJECT_LENGTH: usize = 160;
const MAX_TICKET_ID_LENGTH: usize = 200;
const MAX_JSON_LENGTH: usize = 4096;
const MAX_TTL_SECONDS: u32 = 24 * 60 * 60;
const MAX_MATCH_SIZE: u32 = 128;
const MAX_MATCH_CANDIDATES: usize = 5000;
const SWEEP_BATCH: usize = 500;
const MAX_VIEW_ROWS: usize = 500;
const ONE_SECOND_MICROS: i64 = 1_000_000;
const SWEEP_INTERVAL_MICROS: i64 = 15 * ONE_SECOND_MICROS;

/// The `lobby.*` codes.
pub mod errors {
    pub const NOT_AUTHORIZED: &str = "lobby.not_authorized";
    pub const CANNOT_REMOVE_LAST_ADMIN: &str = "lobby.cannot_remove_last_admin";
    pub const CONFIG_MISSING: &str = "lobby.config_missing";
    pub const INVALID_POOL: &str = "lobby.invalid_pool";
    pub const INVALID_SUBJECT: &str = "lobby.invalid_subject";
    pub const INVALID_TICKET_ID: &str = "lobby.invalid_ticket_id";
    pub const INVALID_WINNER_SUBJECT: &str = "lobby.invalid_winner_subject";
    pub const INVALID_ATTRIBUTES_JSON: &str = "lobby.invalid_attributes_json";
    pub const INVALID_MATCH_SIZE: &str = "lobby.invalid_match_size";
    pub const INVALID_TTL_SECONDS: &str = "lobby.invalid_ttl_seconds";
    pub const INVALID_RATING: &str = "lobby.invalid_rating";
    pub const INVALID_CONFIG: &str = "lobby.invalid_config";
    pub const TICKET_NOT_FOUND: &str = "lobby.ticket_not_found";
    pub const NOT_TICKET_OWNER: &str = "lobby.not_ticket_owner";
    pub const TICKET_NOT_QUEUED: &str = "lobby.ticket_not_queued";
    pub const ROOM_NOT_FOUND: &str = "lobby.room_not_found";
    pub const ROOM_CLOSED: &str = "lobby.room_closed";
    pub const ROOM_NOT_ACTIVE: &str = "lobby.room_not_active";
    pub const SEAT_NOT_FOUND: &str = "lobby.seat_not_found";
    pub const SEAT_LEFT: &str = "lobby.seat_left";
    pub const RESULT_REQUIRES_TWO_SEATS: &str = "lobby.result_requires_two_seats";
    pub const WINNER_NOT_IN_ROOM: &str = "lobby.winner_not_in_room";
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LobbyTicketStatus {
    Queued,
    Matched,
    Cancelled,
    Expired,
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LobbyRoomStatus {
    Ready,
    Active,
    Closed,
    Abandoned,
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LobbySeatStatus {
    Reserved,
    Joined,
    Left,
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

#[spacetimedb::table(accessor = lobby_config)]
#[derive(Clone)]
pub struct LobbyConfig {
    #[primary_key]
    pub singleton: bool,
    pub default_ticket_ttl_seconds: u32,
    pub max_match_size: u32,
    /// Ready rooms whose seats aren't all joined by then are abandoned.
    pub ready_timeout_seconds: u32,
    /// Finished tickets, rooms, seats and results are deleted after this.
    pub retention_seconds: u32,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = lobby_sweep_tick, scheduled(lobby_sweep))]
pub struct LobbySweepTick {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

#[spacetimedb::table(accessor = lobby_admin_identity)]
pub struct LobbyAdminIdentity {
    #[primary_key]
    pub identity: Identity,
    pub added_at: Timestamp,
}

#[spacetimedb::table(
    accessor = lobby_queue_ticket,
    index(accessor = by_status, btree(columns = [status])),
    index(accessor = by_subject, btree(columns = [subject])),
    index(accessor = by_created_at, btree(columns = [created_at])),
    index(accessor = by_status_updated_at, btree(columns = [status, updated_at])),
    index(accessor = by_pool_status_created_at, btree(columns = [pool, status, created_at])),
    index(accessor = by_status_expires_at, btree(columns = [status, expires_at])),
    index(accessor = by_subject_status, btree(columns = [subject, status]))
)]
#[derive(Clone, Debug)]
pub struct LobbyQueueTicket {
    #[primary_key]
    pub ticket_id: String,
    pub pool: String,
    pub subject: String,
    pub status: LobbyTicketStatus,
    pub match_size: u32,
    pub ranked: bool,
    pub rating: Option<i32>,
    pub rating_pool: Option<String>,
    pub party_id: Option<String>,
    pub attributes_json: Option<String>,
    pub room_id: Option<u64>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub expires_at: Timestamp,
}

#[spacetimedb::table(
    accessor = lobby_subject_rating,
    index(accessor = by_pool, btree(columns = [pool])),
    index(accessor = by_subject, btree(columns = [subject])),
    index(accessor = by_rating, btree(columns = [rating])),
    index(accessor = by_leaderboard_order, btree(columns = [pool, rating_order, subject])),
    index(accessor = by_updated_at, btree(columns = [updated_at]))
)]
#[derive(Clone, Debug)]
pub struct LobbySubjectRating {
    #[primary_key]
    pub rating_id: String,
    pub pool: String,
    pub subject: String,
    pub rating: i32,
    pub rating_order: i64,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
    pub matches: u32,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(
    accessor = lobby_room,
    index(accessor = by_status, btree(columns = [status])),
    index(accessor = by_pool, btree(columns = [pool])),
    index(accessor = by_created_at, btree(columns = [created_at])),
    index(accessor = by_status_updated_at, btree(columns = [status, updated_at]))
)]
#[derive(Clone, Debug)]
pub struct LobbyRoom {
    #[primary_key]
    #[auto_inc]
    pub room_id: u64,
    pub pool: String,
    pub status: LobbyRoomStatus,
    pub capacity: u32,
    pub metadata_json: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub closed_at: Option<Timestamp>,
}

#[spacetimedb::table(
    accessor = lobby_match_result,
    index(accessor = by_room, btree(columns = [room_id])),
    index(accessor = by_pool, btree(columns = [pool])),
    index(accessor = by_reported_at, btree(columns = [reported_at]))
)]
#[derive(Clone, Debug)]
pub struct LobbyMatchResult {
    #[primary_key]
    #[auto_inc]
    pub result_id: u64,
    pub room_id: u64,
    pub pool: String,
    pub winner_subject: Option<String>,
    pub loser_subject: Option<String>,
    pub subject_a: String,
    pub subject_b: String,
    pub rating_a_before: i32,
    pub rating_a_after: i32,
    pub rating_b_before: i32,
    pub rating_b_after: i32,
    pub reported_at: Timestamp,
}

#[spacetimedb::table(
    accessor = lobby_room_seat,
    index(accessor = by_room, btree(columns = [room_id])),
    index(accessor = by_subject, btree(columns = [subject])),
    index(accessor = by_status, btree(columns = [status])),
    index(accessor = by_room_subject, btree(columns = [room_id, subject]))
)]
#[derive(Clone, Debug)]
pub struct LobbyRoomSeat {
    #[primary_key]
    #[auto_inc]
    pub seat_id: u64,
    pub room_id: u64,
    pub subject: String,
    pub ticket_id: Option<String>,
    pub seat_index: u32,
    pub status: LobbySeatStatus,
    pub joined_at: Option<Timestamp>,
    pub left_at: Option<Timestamp>,
    pub updated_at: Timestamp,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct LobbyQueueSummaryRow {
    pub pool: String,
    pub queued_tickets: u32,
    pub ready_rooms: u32,
    pub active_rooms: u32,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct LobbyStatus {
    pub default_ticket_ttl_seconds: u32,
    pub max_match_size: u32,
    pub queued_tickets: u32,
    pub ready_rooms: u32,
    pub active_rooms: u32,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct LobbyRankedRatingRow {
    pub pool: String,
    pub subject: String,
    pub rating: i32,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
    pub matches: u32,
}

/// What the queue helpers return: the new ticket, and the room if it matched at once.
#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct LobbyJoinQueueResult {
    pub ticket_id: String,
    pub room_id: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct JoinQueueArgs {
    pub pool: String,
    pub subject: String,
    pub match_size: u32,
    pub attributes_json: Option<String>,
    pub ttl_seconds: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct JoinRankedQueueArgs {
    pub pool: String,
    pub subject: String,
    pub match_size: u32,
    pub attributes_json: Option<String>,
    pub ttl_seconds: Option<u32>,
    /// The rating pool the result updates. Defaults to `pool`.
    pub rating_pool: Option<String>,
}

// ---------------------------------------------------------------------------
// Keys and matchmaking (src/keys.ts, src/matchmaking.ts)
// ---------------------------------------------------------------------------

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Each part length-prefixed (UTF-16 lengths, as in TS), so delimiters can't collide.
pub fn lobby_composite_key(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| format!("{}:{}", utf16_len(p), p))
        .collect()
}

/// How far from the anchor's rating a ranked match may reach: 100, plus 50 per 10 s waited,
/// up to 800.
pub fn ranked_band(created_at: Timestamp, now_micros: i64) -> i64 {
    let created = created_at.to_micros_since_unix_epoch();
    let waited_seconds = if created >= now_micros {
        0
    } else {
        (now_micros - created) / ONE_SECOND_MICROS
    };
    RANKED_MAX_BAND
        .min(RANKED_INITIAL_BAND + (waited_seconds / RANKED_BAND_STEP_SECONDS) * RANKED_BAND_STEP)
}

fn rating_of(t: &LobbyQueueTicket) -> i64 {
    t.rating.unwrap_or(DEFAULT_RATING) as i64
}

/// Picks `match_size` tickets for the oldest anchor that can fill a match inside its rating
/// band, taking the closest ratings first. `queued` is oldest first; returns indexes into it.
pub fn ranked_selection(
    queued: &[LobbyQueueTicket],
    match_size: usize,
    now_micros: i64,
) -> Option<Vec<usize>> {
    let is_older = |a: usize, b: usize| queued[a].created_at < queued[b].created_at;
    // Pools in first-seen order, each sorted by rating then age (a stable sort, as in JS).
    let mut pools: Vec<(Option<&str>, Vec<usize>)> = Vec::new();
    for (i, t) in queued.iter().enumerate() {
        match pools
            .iter_mut()
            .find(|(p, _)| *p == t.rating_pool.as_deref())
        {
            Some((_, v)) => v.push(i),
            None => pools.push((t.rating_pool.as_deref(), vec![i])),
        }
    }
    let mut position = vec![0usize; queued.len()];
    let mut pool_of = vec![0usize; queued.len()];
    for (pi, (_, pool)) in pools.iter_mut().enumerate() {
        pool.sort_by(|&a, &b| {
            rating_of(&queued[a])
                .cmp(&rating_of(&queued[b]))
                .then_with(|| {
                    if is_older(a, b) {
                        Ordering::Less
                    } else if is_older(b, a) {
                        Ordering::Greater
                    } else {
                        Ordering::Equal
                    }
                })
        });
        for (idx, &t) in pool.iter().enumerate() {
            position[t] = idx;
            pool_of[t] = pi;
        }
    }
    for anchor in 0..queued.len() {
        let pool = &pools[pool_of[anchor]].1;
        if pool.len() < match_size {
            continue;
        }
        let anchor_rating = rating_of(&queued[anchor]);
        let band = ranked_band(queued[anchor].created_at, now_micros);
        let mut picked = vec![anchor];
        let mut low = position[anchor] as i64 - 1;
        let mut high = position[anchor] + 1;
        while picked.len() < match_size {
            let left = (low >= 0).then(|| pool[low as usize]);
            let right = (high < pool.len()).then(|| pool[high]);
            let left_gap = left.map_or(i64::MAX, |l| anchor_rating - rating_of(&queued[l]));
            let right_gap = right.map_or(i64::MAX, |r| rating_of(&queued[r]) - anchor_rating);
            if left_gap.min(right_gap) > band {
                break;
            }
            if left_gap < right_gap
                || (left_gap == right_gap && is_older(left.unwrap(), right.unwrap()))
            {
                picked.push(left.unwrap());
                low -= 1;
            } else {
                picked.push(right.unwrap());
                high += 1;
            }
        }
        if picked.len() == match_size {
            return Some(picked);
        }
    }
    None
}

pub fn expected_score(rating: i32, opponent_rating: i32) -> f64 {
    1.0 / (1.0 + 10f64.powf((opponent_rating - rating) as f64 / 400.0))
}

/// `Math.round` (half up), clamped to 100..=5000.
pub fn updated_rating(rating: i32, expected: f64, score: f64) -> i32 {
    let next = (rating as f64 + ELO_K * (score - expected) + 0.5).floor();
    (next as i64).clamp(MIN_RATING as i64, MAX_RATING as i64) as i32
}

// ---------------------------------------------------------------------------
// Operations (src/submodule/operations.ts)
// ---------------------------------------------------------------------------

fn normalize_name(value: &str, code: &str, max: usize) -> Result<String, String> {
    let out = js_trim(value);
    if out.is_empty() || utf16_len(out) > max {
        return Err(code.into());
    }
    Ok(out.to_string())
}

/// JavaScript's `String.prototype.trim` whitespace.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(c, '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
    })
}

/// A JSON object (not null, not an array); returned trimmed but otherwise as given.
fn validate_attributes_json(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(out) = value.map(js_trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if utf16_len(out) > MAX_JSON_LENGTH || !json_is_object(out) {
        return Err(errors::INVALID_ATTRIBUTES_JSON.into());
    }
    Ok(Some(out.to_string()))
}

fn rating_id(pool: &str, subject: &str) -> String {
    lobby_composite_key(&[pool, subject])
}

fn get_or_create_rating(ctx: &ReducerContext, pool: &str, subject: &str) -> LobbySubjectRating {
    let id = rating_id(pool, subject);
    if let Some(existing) = ctx.db.lobby_subject_rating().rating_id().find(&id) {
        return existing;
    }
    ctx.db.lobby_subject_rating().insert(LobbySubjectRating {
        rating_id: id,
        pool: pool.into(),
        subject: subject.into(),
        rating: DEFAULT_RATING,
        rating_order: -(DEFAULT_RATING as i64),
        wins: 0,
        losses: 0,
        draws: 0,
        matches: 0,
        updated_at: ctx.timestamp,
    })
}

fn get_config(ctx: &ReducerContext) -> Result<LobbyConfig, String> {
    ctx.db
        .lobby_config()
        .singleton()
        .find(true)
        .ok_or_else(|| errors::CONFIG_MISSING.into())
}

pub fn is_admin(ctx: &ReducerContext, identity: Identity) -> bool {
    ctx.db
        .lobby_admin_identity()
        .identity()
        .find(identity)
        .is_some()
}

/// `Err(lobby.not_authorized)` unless the caller is a lobby admin.
pub fn require_admin(ctx: &ReducerContext) -> Result<(), String> {
    if is_admin(ctx, ctx.sender()) {
        Ok(())
    } else {
        Err(errors::NOT_AUTHORIZED.into())
    }
}

fn is_terminal_room(room: &LobbyRoom) -> bool {
    matches!(
        room.status,
        LobbyRoomStatus::Closed | LobbyRoomStatus::Abandoned
    )
}

fn seats_for_room(ctx: &ReducerContext, room_id: u64) -> Vec<LobbyRoomSeat> {
    ctx.db
        .lobby_room_seat()
        .by_room()
        .filter(room_id)
        .take(MAX_MATCH_SIZE as usize)
        .collect()
}

fn find_seat(ctx: &ReducerContext, room_id: u64, subject: &str) -> Option<LobbyRoomSeat> {
    ctx.db
        .lobby_room_seat()
        .by_room_subject()
        .filter((room_id, subject))
        .next()
}

fn finish_room(ctx: &ReducerContext, room: LobbyRoom, status: LobbyRoomStatus) {
    ctx.db.lobby_room().room_id().update(LobbyRoom {
        status,
        updated_at: ctx.timestamp,
        closed_at: Some(ctx.timestamp),
        ..room
    });
}

fn attempt_match(ctx: &ReducerContext, pool: &str, match_size: u32, ranked: bool) -> Option<u64> {
    let now = ctx.timestamp;
    let queued: Vec<LobbyQueueTicket> = ctx
        .db
        .lobby_queue_ticket()
        .by_pool_status_created_at()
        .filter((pool, LobbyTicketStatus::Queued))
        .take(MAX_MATCH_CANDIDATES)
        .filter(|t| t.ranked == ranked && t.match_size == match_size && t.expires_at > now)
        .collect();
    if queued.len() < match_size as usize {
        return None;
    }
    let selected: Vec<usize> = if ranked {
        ranked_selection(
            &queued,
            match_size as usize,
            now.to_micros_since_unix_epoch(),
        )?
    } else {
        (0..match_size as usize).collect()
    };
    let room = ctx.db.lobby_room().insert(LobbyRoom {
        room_id: 0,
        pool: pool.into(),
        status: LobbyRoomStatus::Ready,
        capacity: match_size,
        metadata_json: ranked.then(|| {
            let rating_pool = queued[selected[0]].rating_pool.as_deref().unwrap_or(pool);
            format!(
                "{{\"ranked\":true,\"ratingPool\":{}}}",
                json_quote(rating_pool)
            )
        }),
        created_at: now,
        updated_at: now,
        closed_at: None,
    });
    for (index, &i) in selected.iter().enumerate() {
        let ticket = &queued[i];
        ctx.db.lobby_room_seat().insert(LobbyRoomSeat {
            seat_id: 0,
            room_id: room.room_id,
            subject: ticket.subject.clone(),
            ticket_id: Some(ticket.ticket_id.clone()),
            seat_index: index as u32,
            status: LobbySeatStatus::Reserved,
            joined_at: None,
            left_at: None,
            updated_at: now,
        });
        ctx.db
            .lobby_queue_ticket()
            .ticket_id()
            .update(LobbyQueueTicket {
                status: LobbyTicketStatus::Matched,
                room_id: Some(room.room_id),
                updated_at: now,
                ..ticket.clone()
            });
    }
    Some(room.room_id)
}

fn enqueue(
    ctx: &ReducerContext,
    pool: &str,
    subject: &str,
    match_size: u32,
    attributes_json: Option<&str>,
    ttl_seconds: Option<u32>,
    ranked_pool: Option<&str>,
) -> Result<LobbyJoinQueueResult, String> {
    let config = get_config(ctx)?;
    let pool = normalize_name(pool, errors::INVALID_POOL, MAX_POOL_LENGTH)?;
    let subject = normalize_name(subject, errors::INVALID_SUBJECT, MAX_SUBJECT_LENGTH)?;
    if !(1..=config.max_match_size).contains(&match_size) {
        return Err(errors::INVALID_MATCH_SIZE.into());
    }
    let ttl = ttl_seconds.unwrap_or(config.default_ticket_ttl_seconds);
    if !(1..=MAX_TTL_SECONDS).contains(&ttl) {
        return Err(errors::INVALID_TTL_SECONDS.into());
    }
    let attributes_json = validate_attributes_json(attributes_json)?;
    let rating_pool = ranked_pool
        .map(|p| normalize_name(p, errors::INVALID_POOL, MAX_POOL_LENGTH))
        .transpose()?;

    // A subject waits in one queue at a time.
    let waiting: Vec<LobbyQueueTicket> = ctx
        .db
        .lobby_queue_ticket()
        .by_subject_status()
        .filter((subject.as_str(), LobbyTicketStatus::Queued))
        .collect();
    for ticket in waiting {
        ctx.db
            .lobby_queue_ticket()
            .ticket_id()
            .update(LobbyQueueTicket {
                status: LobbyTicketStatus::Cancelled,
                updated_at: ctx.timestamp,
                ..ticket
            });
    }

    let uuid = ctx.new_uuid_v7().map_err(|e| e.to_string())?;
    let ticket_id = format!("ticket:{uuid}");
    let rating = rating_pool.as_deref().map(|rp| {
        ctx.db
            .lobby_subject_rating()
            .rating_id()
            .find(rating_id(rp, &subject))
            .map_or(DEFAULT_RATING, |r| r.rating)
    });
    ctx.db.lobby_queue_ticket().insert(LobbyQueueTicket {
        ticket_id: ticket_id.clone(),
        pool: pool.clone(),
        subject,
        status: LobbyTicketStatus::Queued,
        match_size,
        ranked: ranked_pool.is_some(),
        rating,
        rating_pool,
        party_id: None,
        attributes_json,
        room_id: None,
        created_at: ctx.timestamp,
        updated_at: ctx.timestamp,
        expires_at: ctx.timestamp + TimeDuration::from_micros(ttl as i64 * ONE_SECOND_MICROS),
    });
    let room_id = attempt_match(ctx, &pool, match_size, ranked_pool.is_some());
    Ok(LobbyJoinQueueResult { ticket_id, room_id })
}

/// Queues `subject` for an unranked match, replacing its queued ticket.
pub fn join_queue_for_subject(
    ctx: &ReducerContext,
    args: &JoinQueueArgs,
) -> Result<LobbyJoinQueueResult, String> {
    enqueue(
        ctx,
        &args.pool,
        &args.subject,
        args.match_size,
        args.attributes_json.as_deref(),
        args.ttl_seconds,
        None,
    )
}

/// Queues `subject` for a ranked match. Rating pools appear on the public leaderboard, so only
/// the host chooses them.
pub fn join_ranked_queue_for_subject(
    ctx: &ReducerContext,
    args: &JoinRankedQueueArgs,
) -> Result<LobbyJoinQueueResult, String> {
    let rating_pool = args.rating_pool.as_deref().unwrap_or(&args.pool);
    enqueue(
        ctx,
        &args.pool,
        &args.subject,
        args.match_size,
        args.attributes_json.as_deref(),
        args.ttl_seconds,
        Some(rating_pool),
    )
}

/// The rating pool a ranked room recorded, or its pool.
fn rating_pool_for_room(room: &LobbyRoom) -> String {
    room.metadata_json
        .as_deref()
        .and_then(json_rating_pool)
        .map(|p| js_trim(&p).to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| room.pool.clone())
}

fn apply_result_row(
    ctx: &ReducerContext,
    row: LobbySubjectRating,
    score: f64,
    opponent_rating: i32,
) -> LobbySubjectRating {
    let next_rating = updated_rating(
        row.rating,
        expected_score(row.rating, opponent_rating),
        score,
    );
    let next = LobbySubjectRating {
        rating: next_rating,
        rating_order: -(next_rating as i64),
        wins: row.wins + (score == 1.0) as u32,
        losses: row.losses + (score == 0.0) as u32,
        draws: row.draws + (score == 0.5) as u32,
        matches: row.matches + 1,
        updated_at: ctx.timestamp,
        ..row
    };
    ctx.db
        .lobby_subject_rating()
        .rating_id()
        .update(next.clone());
    next
}

/// Records the result of an active two-player room (`None` = a draw), updates both Elo
/// ratings, and closes the room. Seats that left still get the result.
pub fn report_match_result(
    ctx: &ReducerContext,
    room_id: u64,
    winner_subject: Option<&str>,
) -> Result<(), String> {
    let room = ctx
        .db
        .lobby_room()
        .room_id()
        .find(room_id)
        .ok_or(errors::ROOM_NOT_FOUND)?;
    if room.status != LobbyRoomStatus::Active {
        return Err(errors::ROOM_NOT_ACTIVE.into());
    }
    let mut seats = seats_for_room(ctx, room_id);
    if seats.len() != 2 {
        return Err(errors::RESULT_REQUIRES_TWO_SEATS.into());
    }
    seats.sort_by_key(|s| s.seat_index);
    let (a, b) = (seats[0].subject.clone(), seats[1].subject.clone());
    let winner = winner_subject
        .map(|w| normalize_name(w, errors::INVALID_WINNER_SUBJECT, MAX_SUBJECT_LENGTH))
        .transpose()?;
    if winner.as_ref().is_some_and(|w| *w != a && *w != b) {
        return Err(errors::WINNER_NOT_IN_ROOM.into());
    }
    let score_a = match &winner {
        None => 0.5,
        Some(w) if *w == a => 1.0,
        Some(_) => 0.0,
    };
    let rating_pool = rating_pool_for_room(&room);
    let rating_a = get_or_create_rating(ctx, &rating_pool, &a);
    let rating_b = get_or_create_rating(ctx, &rating_pool, &b);
    let (before_a, before_b) = (rating_a.rating, rating_b.rating);
    let next_a = apply_result_row(ctx, rating_a, score_a, before_b);
    let next_b = apply_result_row(ctx, rating_b, 1.0 - score_a, before_a);
    ctx.db.lobby_match_result().insert(LobbyMatchResult {
        result_id: 0,
        room_id,
        pool: rating_pool,
        loser_subject: winner
            .as_ref()
            .map(|w| if *w == a { b.clone() } else { a.clone() }),
        winner_subject: winner,
        subject_a: a,
        subject_b: b,
        rating_a_before: before_a,
        rating_a_after: next_a.rating,
        rating_b_before: before_b,
        rating_b_after: next_b.rating,
        reported_at: ctx.timestamp,
    });
    finish_room(ctx, room, LobbyRoomStatus::Closed);
    Ok(())
}

pub fn cancel_ticket_for_subject(
    ctx: &ReducerContext,
    ticket_id: &str,
    subject: &str,
) -> Result<(), String> {
    let ticket_id = normalize_name(ticket_id, errors::INVALID_TICKET_ID, MAX_TICKET_ID_LENGTH)?;
    let ticket = ctx
        .db
        .lobby_queue_ticket()
        .ticket_id()
        .find(&ticket_id)
        .ok_or(errors::TICKET_NOT_FOUND)?;
    if ticket.subject != subject {
        return Err(errors::NOT_TICKET_OWNER.into());
    }
    if ticket.status != LobbyTicketStatus::Queued {
        return Err(errors::TICKET_NOT_QUEUED.into());
    }
    ctx.db
        .lobby_queue_ticket()
        .ticket_id()
        .update(LobbyQueueTicket {
            status: LobbyTicketStatus::Cancelled,
            updated_at: ctx.timestamp,
            ..ticket
        });
    Ok(())
}

/// Marks `subject`'s reserved seat joined. The room activates once every seat joins.
pub fn join_room_for_subject(
    ctx: &ReducerContext,
    room_id: u64,
    subject: &str,
) -> Result<(), String> {
    let room = ctx
        .db
        .lobby_room()
        .room_id()
        .find(room_id)
        .ok_or(errors::ROOM_NOT_FOUND)?;
    if is_terminal_room(&room) {
        return Err(errors::ROOM_CLOSED.into());
    }
    let seat = find_seat(ctx, room_id, subject).ok_or(errors::SEAT_NOT_FOUND)?;
    match seat.status {
        LobbySeatStatus::Left => return Err(errors::SEAT_LEFT.into()),
        LobbySeatStatus::Joined => return Ok(()),
        LobbySeatStatus::Reserved => {}
    }
    ctx.db.lobby_room_seat().seat_id().update(LobbyRoomSeat {
        status: LobbySeatStatus::Joined,
        joined_at: Some(ctx.timestamp),
        updated_at: ctx.timestamp,
        ..seat
    });
    if room.status == LobbyRoomStatus::Ready
        && seats_for_room(ctx, room_id)
            .iter()
            .all(|s| s.status == LobbySeatStatus::Joined)
    {
        ctx.db.lobby_room().room_id().update(LobbyRoom {
            status: LobbyRoomStatus::Active,
            updated_at: ctx.timestamp,
            ..room
        });
    }
    Ok(())
}

/// Marks `subject`'s seat left. Leaving a ready room abandons it. An active room stays active
/// (so the host can still report its result) until every seat has left.
pub fn leave_room_for_subject(
    ctx: &ReducerContext,
    room_id: u64,
    subject: &str,
) -> Result<(), String> {
    let room = ctx
        .db
        .lobby_room()
        .room_id()
        .find(room_id)
        .ok_or(errors::ROOM_NOT_FOUND)?;
    let seat = find_seat(ctx, room_id, subject).ok_or(errors::SEAT_NOT_FOUND)?;
    if seat.status == LobbySeatStatus::Left || is_terminal_room(&room) {
        return Ok(());
    }
    ctx.db.lobby_room_seat().seat_id().update(LobbyRoomSeat {
        status: LobbySeatStatus::Left,
        left_at: Some(ctx.timestamp),
        updated_at: ctx.timestamp,
        ..seat
    });
    if room.status == LobbyRoomStatus::Ready
        || seats_for_room(ctx, room_id)
            .iter()
            .all(|s| s.status == LobbySeatStatus::Left)
    {
        finish_room(ctx, room, LobbyRoomStatus::Abandoned);
    }
    Ok(())
}

/// Closes a room without a result. Use [`report_match_result`] to finish a ranked match.
pub fn close_room(ctx: &ReducerContext, room_id: u64) -> Result<(), String> {
    let room = ctx
        .db
        .lobby_room()
        .room_id()
        .find(room_id)
        .ok_or(errors::ROOM_NOT_FOUND)?;
    if !is_terminal_room(&room) {
        finish_room(ctx, room, LobbyRoomStatus::Closed);
    }
    Ok(())
}

fn older_than(ctx: &ReducerContext, seconds: u32) -> Timestamp {
    Timestamp::from_micros_since_unix_epoch(
        ctx.timestamp.to_micros_since_unix_epoch() - seconds as i64 * ONE_SECOND_MICROS,
    )
}

fn ts_min() -> Timestamp {
    Timestamp::from_micros_since_unix_epoch(i64::MIN)
}

fn delete_room(ctx: &ReducerContext, room: &LobbyRoom) {
    for seat in seats_for_room(ctx, room.room_id) {
        if let Some(ticket_id) = &seat.ticket_id {
            ctx.db.lobby_queue_ticket().ticket_id().delete(ticket_id);
        }
        ctx.db.lobby_room_seat().seat_id().delete(seat.seat_id);
    }
    ctx.db.lobby_room().room_id().delete(room.room_id);
}

/// One bounded cleanup pass (runs on the schedule [`install`] creates; public for hosts and
/// tests).
pub fn sweep(ctx: &ReducerContext) -> Result<(), String> {
    let config = get_config(ctx)?;
    let now = ctx.timestamp;

    let expired: Vec<LobbyQueueTicket> = ctx
        .db
        .lobby_queue_ticket()
        .by_status_expires_at()
        .filter((LobbyTicketStatus::Queued, ts_min()..=now))
        .take(SWEEP_BATCH)
        .collect();
    for ticket in expired {
        ctx.db
            .lobby_queue_ticket()
            .ticket_id()
            .update(LobbyQueueTicket {
                status: LobbyTicketStatus::Expired,
                updated_at: now,
                ..ticket
            });
    }

    let retained = older_than(ctx, config.retention_seconds);
    for status in [LobbyTicketStatus::Cancelled, LobbyTicketStatus::Expired] {
        let old: Vec<String> = ctx
            .db
            .lobby_queue_ticket()
            .by_status_updated_at()
            .filter((status, ts_min()..=retained))
            .take(SWEEP_BATCH)
            .map(|t| t.ticket_id)
            .collect();
        for id in &old {
            ctx.db.lobby_queue_ticket().ticket_id().delete(id);
        }
    }

    let stale: Vec<LobbyRoom> = ctx
        .db
        .lobby_room()
        .by_status_updated_at()
        .filter((
            LobbyRoomStatus::Ready,
            ts_min()..=older_than(ctx, config.ready_timeout_seconds),
        ))
        .take(SWEEP_BATCH)
        .collect();
    for room in stale {
        finish_room(ctx, room, LobbyRoomStatus::Abandoned);
    }

    for status in [LobbyRoomStatus::Closed, LobbyRoomStatus::Abandoned] {
        let old: Vec<LobbyRoom> = ctx
            .db
            .lobby_room()
            .by_status_updated_at()
            .filter((status, ts_min()..=retained))
            .take(SWEEP_BATCH)
            .collect();
        for room in &old {
            delete_room(ctx, room);
        }
    }

    let old_results: Vec<u64> = ctx
        .db
        .lobby_match_result()
        .by_reported_at()
        .filter(ts_min()..=retained)
        .take(SWEEP_BATCH)
        .map(|r| r.result_id)
        .collect();
    for id in &old_results {
        ctx.db.lobby_match_result().result_id().delete(id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Install, client reducers, procedure
// ---------------------------------------------------------------------------

/// Call from your `init` reducer: seeds the default config (60 s tickets, matches of up to
/// 16, 120 s ready timeout, 1 h retention), makes the caller a lobby admin, and schedules the
/// cleanup sweep every 15 s.
pub fn install(ctx: &ReducerContext) {
    if ctx.db.lobby_config().singleton().find(true).is_none() {
        ctx.db.lobby_config().insert(LobbyConfig {
            singleton: true,
            default_ticket_ttl_seconds: 60,
            max_match_size: 16,
            ready_timeout_seconds: 120,
            retention_seconds: 60 * 60,
            updated_at: ctx.timestamp,
        });
    }
    if ctx
        .db
        .lobby_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        ctx.db.lobby_admin_identity().insert(LobbyAdminIdentity {
            identity: ctx.sender(),
            added_at: ctx.timestamp,
        });
    }
    if ctx.db.lobby_sweep_tick().count() == 0 {
        ctx.db.lobby_sweep_tick().insert(LobbySweepTick {
            scheduled_id: 0,
            scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(SWEEP_INTERVAL_MICROS)),
        });
    }
}

fn sender_subject(ctx: &ReducerContext) -> String {
    ctx.sender().to_hex().to_string()
}

/// Queue the caller (their identity, as hex) for an unranked match. TS: `join_queue`.
#[spacetimedb::reducer]
pub fn lobby_join_queue(
    ctx: &ReducerContext,
    pool: String,
    match_size: u32,
    attributes_json: Option<String>,
    ttl_seconds: Option<u32>,
) -> Result<(), String> {
    join_queue_for_subject(
        ctx,
        &JoinQueueArgs {
            pool,
            subject: sender_subject(ctx),
            match_size,
            attributes_json,
            ttl_seconds,
        },
    )
    .map(|_| ())
}

/// TS: `cancel_ticket`.
#[spacetimedb::reducer]
pub fn lobby_cancel_ticket(ctx: &ReducerContext, ticket_id: String) -> Result<(), String> {
    cancel_ticket_for_subject(ctx, &ticket_id, &sender_subject(ctx))
}

/// TS: `join_room`.
#[spacetimedb::reducer]
pub fn lobby_join_room(ctx: &ReducerContext, room_id: u64) -> Result<(), String> {
    join_room_for_subject(ctx, room_id, &sender_subject(ctx))
}

/// TS: `leave_room`.
#[spacetimedb::reducer]
pub fn lobby_leave_room(ctx: &ReducerContext, room_id: u64) -> Result<(), String> {
    leave_room_for_subject(ctx, room_id, &sender_subject(ctx))
}

/// Admins. TS: `set_rating`.
#[spacetimedb::reducer]
pub fn lobby_set_rating(
    ctx: &ReducerContext,
    pool: String,
    subject: String,
    rating: i32,
) -> Result<(), String> {
    require_admin(ctx)?;
    let pool = normalize_name(&pool, errors::INVALID_POOL, MAX_POOL_LENGTH)?;
    let subject = normalize_name(&subject, errors::INVALID_SUBJECT, MAX_SUBJECT_LENGTH)?;
    if !(MIN_RATING..=MAX_RATING).contains(&rating) {
        return Err(errors::INVALID_RATING.into());
    }
    let existing = get_or_create_rating(ctx, &pool, &subject);
    ctx.db
        .lobby_subject_rating()
        .rating_id()
        .update(LobbySubjectRating {
            rating,
            rating_order: -(rating as i64),
            updated_at: ctx.timestamp,
            ..existing
        });
    Ok(())
}

/// Admins. TS: `update_config`.
#[spacetimedb::reducer]
pub fn lobby_update_config(
    ctx: &ReducerContext,
    default_ticket_ttl_seconds: u32,
    max_match_size: u32,
    ready_timeout_seconds: u32,
    retention_seconds: u32,
) -> Result<(), String> {
    require_admin(ctx)?;
    if !(1..=MAX_TTL_SECONDS).contains(&default_ticket_ttl_seconds)
        || !(1..=MAX_MATCH_SIZE).contains(&max_match_size)
        || ready_timeout_seconds < 1
        || retention_seconds < 1
    {
        return Err(errors::INVALID_CONFIG.into());
    }
    let config = get_config(ctx)?;
    ctx.db.lobby_config().singleton().update(LobbyConfig {
        default_ticket_ttl_seconds,
        max_match_size,
        ready_timeout_seconds,
        retention_seconds,
        updated_at: ctx.timestamp,
        ..config
    });
    Ok(())
}

#[spacetimedb::reducer]
pub fn add_lobby_admin(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .lobby_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        ctx.db.lobby_admin_identity().insert(LobbyAdminIdentity {
            identity,
            added_at: ctx.timestamp,
        });
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn remove_lobby_admin(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .lobby_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        return Ok(());
    }
    if ctx.db.lobby_admin_identity().count() <= 1 {
        return Err(errors::CANNOT_REMOVE_LAST_ADMIN.into());
    }
    ctx.db.lobby_admin_identity().identity().delete(identity);
    Ok(())
}

/// Every 15 s: expire tickets, abandon ready rooms past the timeout, delete what's past the
/// retention window.
#[spacetimedb::reducer]
pub fn lobby_sweep(ctx: &ReducerContext, _tick: LobbySweepTick) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    sweep(ctx)
}

#[spacetimedb::procedure]
pub fn get_lobby_status(ctx: &mut ProcedureContext) -> Result<LobbyStatus, String> {
    ctx.try_with_tx(|tx| {
        let config = get_config(tx)?;
        Ok(LobbyStatus {
            default_ticket_ttl_seconds: config.default_ticket_ttl_seconds,
            max_match_size: config.max_match_size,
            queued_tickets: tx
                .db
                .lobby_queue_ticket()
                .by_status()
                .filter(LobbyTicketStatus::Queued)
                .take(MAX_MATCH_CANDIDATES)
                .count() as u32,
            ready_rooms: tx
                .db
                .lobby_room()
                .by_status()
                .filter(LobbyRoomStatus::Ready)
                .take(MAX_MATCH_CANDIDATES)
                .count() as u32,
            active_rooms: tx
                .db
                .lobby_room()
                .by_status()
                .filter(LobbyRoomStatus::Active)
                .take(MAX_MATCH_CANDIDATES)
                .count() as u32,
        })
    })
}

// ---------------------------------------------------------------------------
// Views (src/submodule/views.ts)
// ---------------------------------------------------------------------------

/// Insert each row after any equal ones and keep the first `limit` (TS `selectTopRows`).
fn select_top_rows<T>(
    rows: impl Iterator<Item = T>,
    limit: usize,
    cmp: impl Fn(&T, &T) -> Ordering,
) -> Vec<T> {
    let mut selected: Vec<T> = Vec::new();
    for row in rows {
        let at = selected.partition_point(|s| cmp(&row, s) != Ordering::Less);
        if at >= limit {
            continue;
        }
        selected.insert(at, row);
        selected.truncate(limit);
    }
    selected
}

fn newest_first(a: Timestamp, b: Timestamp) -> Ordering {
    b.cmp(&a)
}

fn view_subject(ctx: &ViewContext) -> String {
    ctx.sender().to_hex().to_string()
}

fn view_is_admin(ctx: &ViewContext) -> bool {
    ctx.db
        .lobby_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_some()
}

fn rating_row(r: LobbySubjectRating) -> LobbyRankedRatingRow {
    LobbyRankedRatingRow {
        pool: r.pool,
        subject: r.subject,
        rating: r.rating,
        wins: r.wins,
        losses: r.losses,
        draws: r.draws,
        matches: r.matches,
    }
}

#[spacetimedb::view(accessor = my_lobby_tickets, public)]
fn my_lobby_tickets(ctx: &ViewContext) -> Vec<LobbyQueueTicket> {
    select_top_rows(
        ctx.db
            .lobby_queue_ticket()
            .by_subject()
            .filter(&view_subject(ctx)),
        MAX_VIEW_ROWS,
        |a, b| newest_first(a.created_at, b.created_at),
    )
}

#[spacetimedb::view(accessor = my_lobby_ratings, public)]
fn my_lobby_ratings(ctx: &ViewContext) -> Vec<LobbyRankedRatingRow> {
    select_top_rows(
        ctx.db
            .lobby_subject_rating()
            .by_subject()
            .filter(&view_subject(ctx)),
        MAX_VIEW_ROWS,
        |a, b| a.pool.cmp(&b.pool),
    )
    .into_iter()
    .map(rating_row)
    .collect()
}

#[spacetimedb::view(accessor = my_lobby_room_seats, public)]
fn my_lobby_room_seats(ctx: &ViewContext) -> Vec<LobbyRoomSeat> {
    select_top_rows(
        ctx.db
            .lobby_room_seat()
            .by_subject()
            .filter(&view_subject(ctx)),
        MAX_VIEW_ROWS,
        |a, b| newest_first(a.updated_at, b.updated_at),
    )
}

#[spacetimedb::view(accessor = my_lobby_rooms, public)]
fn my_lobby_rooms(ctx: &ViewContext) -> Vec<LobbyRoom> {
    let seats = select_top_rows(
        ctx.db
            .lobby_room_seat()
            .by_subject()
            .filter(&view_subject(ctx)),
        MAX_VIEW_ROWS,
        |a, b| newest_first(a.updated_at, b.updated_at),
    );
    let mut seen = HashSet::new();
    let mut rooms: Vec<LobbyRoom> = seats
        .iter()
        .filter(|s| seen.insert(s.room_id))
        .filter_map(|s| ctx.db.lobby_room().room_id().find(s.room_id))
        .collect();
    rooms.sort_by(|a, b| newest_first(a.created_at, b.created_at));
    rooms
}

#[spacetimedb::view(accessor = lobby_queue_summary, public)]
fn lobby_queue_summary(ctx: &ViewContext) -> Vec<LobbyQueueSummaryRow> {
    let mut summary: Vec<LobbyQueueSummaryRow> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut ensure = |pool: &str| -> usize {
        *index.entry(pool.to_string()).or_insert_with(|| {
            summary.push(LobbyQueueSummaryRow {
                pool: pool.into(),
                queued_tickets: 0,
                ready_rooms: 0,
                active_rooms: 0,
            });
            summary.len() - 1
        })
    };
    let mut counts: Vec<(usize, u8)> = Vec::new();
    for t in ctx
        .db
        .lobby_queue_ticket()
        .by_status()
        .filter(LobbyTicketStatus::Queued)
        .take(MAX_MATCH_CANDIDATES)
    {
        counts.push((ensure(&t.pool), 0));
    }
    for r in ctx
        .db
        .lobby_room()
        .by_status()
        .filter(LobbyRoomStatus::Ready)
        .take(MAX_MATCH_CANDIDATES)
    {
        counts.push((ensure(&r.pool), 1));
    }
    for r in ctx
        .db
        .lobby_room()
        .by_status()
        .filter(LobbyRoomStatus::Active)
        .take(MAX_MATCH_CANDIDATES)
    {
        counts.push((ensure(&r.pool), 2));
    }
    for (i, kind) in counts {
        let row = &mut summary[i];
        match kind {
            0 => row.queued_tickets += 1,
            1 => row.ready_rooms += 1,
            _ => row.active_rooms += 1,
        }
    }
    summary.sort_by(|a, b| a.pool.cmp(&b.pool));
    summary
}

#[spacetimedb::view(accessor = lobby_ranked_leaderboard, public)]
fn lobby_ranked_leaderboard(ctx: &ViewContext) -> Vec<LobbyRankedRatingRow> {
    // Walk the whole (pool, rating_order, subject) index: per pool, best first.
    ctx.db
        .lobby_subject_rating()
        .by_leaderboard_order()
        .filter(""..)
        .take(500)
        .map(rating_row)
        .collect()
}

#[spacetimedb::view(accessor = lobby_admin_tickets, public)]
fn lobby_admin_tickets(ctx: &ViewContext) -> Vec<LobbyQueueTicket> {
    if !view_is_admin(ctx) {
        return Vec::new();
    }
    select_top_rows(
        ctx.db
            .lobby_queue_ticket()
            .by_created_at()
            .filter(ts_min()..),
        MAX_VIEW_ROWS,
        |a, b| newest_first(a.created_at, b.created_at),
    )
}

#[spacetimedb::view(accessor = lobby_admin_rooms, public)]
fn lobby_admin_rooms(ctx: &ViewContext) -> Vec<LobbyRoom> {
    if !view_is_admin(ctx) {
        return Vec::new();
    }
    select_top_rows(
        ctx.db.lobby_room().by_created_at().filter(ts_min()..),
        MAX_VIEW_ROWS,
        |a, b| newest_first(a.created_at, b.created_at),
    )
}

#[spacetimedb::view(accessor = lobby_admin_room_seats, public)]
fn lobby_admin_room_seats(ctx: &ViewContext) -> Vec<LobbyRoomSeat> {
    if !view_is_admin(ctx) {
        return Vec::new();
    }
    ctx.db
        .lobby_room_seat()
        .by_room()
        .filter(0u64..)
        .take(1000)
        .collect()
}

#[spacetimedb::view(accessor = lobby_admin_match_results, public)]
fn lobby_admin_match_results(ctx: &ViewContext) -> Vec<LobbyMatchResult> {
    if !view_is_admin(ctx) {
        return Vec::new();
    }
    select_top_rows(
        ctx.db
            .lobby_match_result()
            .by_reported_at()
            .filter(ts_min()..),
        500,
        |a, b| newest_first(a.reported_at, b.reported_at),
    )
}

// ---------------------------------------------------------------------------
// JSON: validate an object, quote a string, read our own room metadata
// ---------------------------------------------------------------------------

/// `JSON.stringify(s)` for a string.
fn json_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Whether `text` is valid JSON (`JSON.parse` rules) whose value is an object.
fn json_is_object(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    let ws = |i: &mut usize| {
        while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
            *i += 1;
        }
    };
    ws(&mut i);
    if b.get(i) != Some(&b'{') || json_skip_value(b, &mut i, 0).is_none() {
        return false;
    }
    ws(&mut i);
    i == b.len()
}

/// Skip one JSON value starting at `*i`.
fn json_skip_value(b: &[u8], i: &mut usize, depth: usize) -> Option<()> {
    let ws = |i: &mut usize| {
        while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
            *i += 1;
        }
    };
    if depth > 512 {
        return None;
    }
    match *b.get(*i)? {
        b'{' | b'[' => {
            let close = if b[*i] == b'{' { b'}' } else { b']' };
            let object = close == b'}';
            *i += 1;
            ws(i);
            if b.get(*i) == Some(&close) {
                *i += 1;
                return Some(());
            }
            loop {
                ws(i);
                if object {
                    if b.get(*i) != Some(&b'"') {
                        return None;
                    }
                    json_skip_string(b, i)?;
                    ws(i);
                    if b.get(*i) != Some(&b':') {
                        return None;
                    }
                    *i += 1;
                    ws(i);
                }
                json_skip_value(b, i, depth + 1)?;
                ws(i);
                match b.get(*i)? {
                    b',' => *i += 1,
                    c if *c == close => {
                        *i += 1;
                        return Some(());
                    }
                    _ => return None,
                }
            }
        }
        b'"' => json_skip_string(b, i),
        b't' => json_skip_lit(b, i, b"true"),
        b'f' => json_skip_lit(b, i, b"false"),
        b'n' => json_skip_lit(b, i, b"null"),
        _ => {
            let digits = |i: &mut usize| {
                let s = *i;
                while *i < b.len() && b[*i].is_ascii_digit() {
                    *i += 1;
                }
                *i - s
            };
            if b.get(*i) == Some(&b'-') {
                *i += 1;
            }
            if b.get(*i) == Some(&b'0') {
                *i += 1;
            } else if digits(i) == 0 {
                return None;
            }
            if b.get(*i) == Some(&b'.') {
                *i += 1;
                if digits(i) == 0 {
                    return None;
                }
            }
            if matches!(b.get(*i), Some(b'e' | b'E')) {
                *i += 1;
                if matches!(b.get(*i), Some(b'+' | b'-')) {
                    *i += 1;
                }
                if digits(i) == 0 {
                    return None;
                }
            }
            Some(())
        }
    }
}

fn json_skip_lit(b: &[u8], i: &mut usize, word: &[u8]) -> Option<()> {
    b.get(*i..*i + word.len()).filter(|s| *s == word)?;
    *i += word.len();
    Some(())
}

fn json_skip_string(b: &[u8], i: &mut usize) -> Option<()> {
    *i += 1;
    loop {
        match *b.get(*i)? {
            b'"' => {
                *i += 1;
                return Some(());
            }
            b'\\' => {
                *i += 1;
                match *b.get(*i)? {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => *i += 1,
                    b'u' => {
                        b.get(*i + 1..*i + 5)
                            .filter(|h| h.iter().all(u8::is_ascii_hexdigit))?;
                        *i += 5;
                    }
                    _ => return None,
                }
            }
            0..=0x1f => return None,
            _ => *i += 1,
        }
    }
}

/// The `ratingPool` string in room metadata this file wrote (`{"ranked":true,"ratingPool":"…"}`).
fn json_rating_pool(metadata: &str) -> Option<String> {
    let rest = metadata.strip_prefix("{\"ranked\":true,\"ratingPool\":\"")?;
    let mut out = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next()? {
            '"' => return (chars.as_str() == "}").then_some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}
