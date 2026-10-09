# lobby: matchmaking queues, rooms, seats and Elo ratings

Port of [`@spacetimedb/lobby`](https://github.com/clockworklabs/SpacetimeDB/pull/5908) at
`f6ed7dcd1c8f853c97fc7e9f126606fca7adadac`.

**Schema:**
- **Tables:** `lobby_config`, `lobby_admin_identity`, `lobby_queue_ticket`, `lobby_subject_rating`, `lobby_room`,
  `lobby_match_result`, `lobby_room_seat` (all private, with every TS index) and `lobby_sweep_tick` (every 15 s).
- **Views:** `my_lobby_tickets`, `my_lobby_ratings`, `my_lobby_room_seats`, `my_lobby_rooms`, `lobby_queue_summary`,
  `lobby_ranked_leaderboard`, and the admin views `lobby_admin_tickets`, `lobby_admin_rooms`, `lobby_admin_room_seats`,
  `lobby_admin_match_results`.
- **Procedure:** `get_lobby_status`.
- **Reducers:**
  - `add_lobby_admin` and `remove_lobby_admin`, plus the scheduled `lobby_sweep`.
  - Prefixed (generic in TS): `lobby_join_queue`, `lobby_cancel_ticket`, `lobby_join_room`, `lobby_leave_room`,
    `lobby_set_rating`, `lobby_update_config`. In TS they're `join_queue`, `cancel_ticket`, … under the submodule's
    namespace.

**Matchmaking matches TS:**
- **Unranked:** the first tickets of equal size.
- **Ranked:** the oldest anchor whose band holds a full match. The band is 100 plus 50 per 10 s waited, capped at
  800, and the closest ratings are taken first.
- **Elo:** K = 32, with ratings clamped to 100..5000.
- **Vectors:** 300 ranked selections, 300 Elo updates and the bands are checked against answers from the real TS code
  ([`tools/gen-lobby-vectors.mjs`](../tools/gen-lobby-vectors.mjs)).

```rust
lobby::install(ctx);   // from init

// Ranked queues and results are host-only: rating pools are public on the leaderboard.
lobby::join_ranked_queue_for_subject(ctx, &lobby::JoinRankedQueueArgs {
    pool: "duel".into(),
    subject: ctx.sender().to_hex().to_string(),
    match_size: 2,
    ..Default::default()
})?;
lobby::report_match_result(ctx, room_id, Some(&winner))?;   // None = a draw
```

```csharp
LobbyInstall(ctx);
LobbyJoinRankedQueueForSubject(LobbyTx.Of(ctx), new JoinRankedQueueArgs("duel", LobbySubjectOf(ctx.Sender), 2));
LobbyReportMatchResult(LobbyTx.Of(ctx), roomId, winner);    // null = a draw
```

| TypeScript | Rust | C# |
|---|---|---|
| `install(ctx)` | `lobby::install(ctx)` | `LobbyInstall(ctx)` |
| `joinQueueForSubject` / `joinRankedQueueForSubject` | `join_queue_for_subject` / `join_ranked_queue_for_subject` | `LobbyJoinQueueForSubject` / `LobbyJoinRankedQueueForSubject` |
| `cancelTicketForSubject`, `joinRoomForSubject`, `leaveRoomForSubject` | `cancel_ticket_for_subject`, `join_room_for_subject`, `leave_room_for_subject` | `LobbyCancelTicketForSubject`, `LobbyJoinRoomForSubject`, `LobbyLeaveRoomForSubject` |
| `reportMatchResult`, `closeRoom` | `report_match_result`, `close_room` | `LobbyReportMatchResult`, `LobbyCloseRoom` |
| `rankedSelection`, `rankedBand`, `expectedScore`, `updatedRating`, `lobbyCompositeKey` | same names, snake_case | `LobbyRankedSelection`, `LobbyRankedBand`, `LobbyExpectedScore`, `LobbyUpdatedRating`, `LobbyCompositeKey` |

**Differences:**
- **Procedure result:** `get_lobby_status` returns `Result<LobbyStatus, String>`.
- **Scheduler-only:** the sweep reducer rejects callers other than the scheduler. It's also available as
  `lobby::sweep` / `LobbySweepNow`.
- **Pool sort order:** views sort pools by plain string order; TS uses `localeCompare`, an ICU collation modules
  don't have. It's the same for ASCII pool names that don't mix cases.
- **Admin seat view:** `lobby_admin_room_seats` walks the `room_id` index (Rust and C# views can't scan tables), so
  rows come out grouped by room.
- **C# context:** C# helpers take a `LobbyTx` (db, timestamp, sender, UUID source), like api-keys.

**Seat order is not strictly oldest first.** Matching reads `by_pool_status_created_at` inside the transaction that
just inserted the newcomer's ticket. SpacetimeDB returns a transaction's own inserts before committed rows (see
[FINDINGS](../docs/FINDINGS.md#range-scans-inside-a-transaction-arent-globally-ordered)), so the newcomer comes
first: it gets seat 0, and in ranked matching it is the first anchor tried. TS runs on the same engine. The ports
keep the engine's order rather than "fixing" it, so they behave like TS, and the tests pin it.

**Tests:** in [`tests/lobby.test.mjs`](../tests/lobby.test.mjs):
- the upstream matchmaking and room tests
- the generated vectors
- end to end: queues, validation, cancel, the room lifecycle (ready, active, abandoned, closed), ranked rating pools
  and draws, the two-seat rule, every view per caller and for admins, config, and the sweep (expire, abandon,
  retention)
