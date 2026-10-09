// Lobby.cs: matchmaking queues, rooms, seats and Elo ratings, for C# modules.
//
// Unofficial C# port of @spacetimedb/lobby (SpacetimeDB PR #5908, commit
// f6ed7dcd1c8f853c97fc7e9f126606fca7adadac, package spacetime-lobby-ts). Licensed Apache-2.0,
// like the original. Original design and behaviour by Clockwork Labs; this port is by Tinker
// (Gazz-Stripbolt).
//
// Same private tables (lobby_config, lobby_admin_identity, lobby_queue_ticket,
// lobby_subject_rating, lobby_room, lobby_match_result, lobby_room_seat, lobby_sweep_tick),
// indexes, limits, matchmaking, Elo (K = 32), lifecycle rules, error codes and views as the
// TS package.
//
//     [Reducer(ReducerKind.Init)]
//     public static void Init(ReducerContext ctx) => LobbyInstall(ctx);
//
//     // Ranked queues and results are host-only (rating pools are public on the leaderboard):
//     [Reducer]
//     public static void QueueRanked(ReducerContext ctx) =>
//         LobbyJoinRankedQueueForSubject(LobbyTx.Of(ctx), new JoinRankedQueueArgs("duel", subjectOf(ctx.Sender), 2));
//
// Changes from the TypeScript original:
// - The generic client reducer names are prefixed: join_queue -> lobby_join_queue,
//   cancel_ticket -> lobby_cancel_ticket, join_room -> lobby_join_room, leave_room ->
//   lobby_leave_room, set_rating -> lobby_set_rating, update_config -> lobby_update_config.
// - get_lobby_status returns Result<LobbyStatus, string>.
// - lobby_sweep rejects callers other than the scheduler.
// - Helpers take a LobbyTx (database, timestamp, sender, UUID source), built from a
//   ReducerContext or from a procedure's context and transaction.
// - Pools sort by ordinal string order in the views (TS uses localeCompare, an ICU collation
//   modules don't have).
// - lobby_admin_room_seats walks the RoomId index (C# views can't scan tables).

#pragma warning disable STDB_UNSTABLE
#nullable enable

using System.Globalization;
using System.Text;
using SpacetimeDB;

[SpacetimeDB.Type]
public enum LobbyTicketStatus
{
    Queued,
    Matched,
    Cancelled,
    Expired,
}

[SpacetimeDB.Type]
public enum LobbyRoomStatus
{
    Ready,
    Active,
    Closed,
    Abandoned,
}

[SpacetimeDB.Type]
public enum LobbySeatStatus
{
    Reserved,
    Joined,
    Left,
}

[SpacetimeDB.Type]
public partial struct LobbyQueueSummaryRow
{
    public string Pool;
    public uint QueuedTickets;
    public uint ReadyRooms;
    public uint ActiveRooms;
}

[SpacetimeDB.Type]
public partial struct LobbyStatus
{
    public uint DefaultTicketTtlSeconds;
    public uint MaxMatchSize;
    public uint QueuedTickets;
    public uint ReadyRooms;
    public uint ActiveRooms;
}

[SpacetimeDB.Type]
public partial struct LobbyRankedRatingRow
{
    public string Pool;
    public string Subject;
    public int Rating;
    public uint Wins;
    public uint Losses;
    public uint Draws;
    public uint Matches;
}

/// <summary>What the queue helpers return: the new ticket, and the room if it matched at once.</summary>
[SpacetimeDB.Type]
public partial struct LobbyJoinQueueResult
{
    public string TicketId;
    public ulong? RoomId;
}

public record JoinQueueArgs(string Pool, string Subject, uint MatchSize)
{
    public string? AttributesJson { get; init; }
    public uint? TtlSeconds { get; init; }
}

public record JoinRankedQueueArgs(string Pool, string Subject, uint MatchSize)
{
    public string? AttributesJson { get; init; }
    public uint? TtlSeconds { get; init; }
    /// <summary>The rating pool the result updates. Defaults to Pool.</summary>
    public string? RatingPool { get; init; }
}

/// <summary>What the helpers need from a transaction: works from reducers and from procedures.</summary>
public readonly record struct LobbyTx(Local Db, Timestamp Timestamp, Identity Sender, Func<Uuid> NewUuidV7)
{
    public static LobbyTx Of(ReducerContext ctx) => new(ctx.Db, ctx.Timestamp, ctx.Sender, ctx.NewUuidV7);
    public static LobbyTx Of(ProcedureContext ctx, ProcedureTxContext tx) => new(tx.Db, tx.Timestamp, tx.Sender, ctx.NewUuidV7);
}

public static partial class Module
{
    public const int LobbyDefaultRating = 1000;
    public const int LobbyMinRating = 100;
    public const int LobbyMaxRating = 5000;
    const double LobbyEloK = 32;
    const long LobbyRankedInitialBand = 100;
    const long LobbyRankedBandStep = 50;
    const long LobbyRankedBandStepSeconds = 10;
    const long LobbyRankedMaxBand = 800;

    const int LobbyMaxPoolLength = 96;
    const int LobbyMaxSubjectLength = 160;
    const int LobbyMaxTicketIdLength = 200;
    const int LobbyMaxJsonLength = 4096;
    const uint LobbyMaxTtlSeconds = 24 * 60 * 60;
    const uint LobbyMaxMatchSize = 128;
    const int LobbyMaxMatchCandidates = 5000;
    const int LobbySweepBatch = 500;
    const int LobbyMaxViewRows = 500;
    const long LobbySweepIntervalMicros = 15_000_000;

    /// <summary>The lobby.* codes.</summary>
    public static class LobbyErrors
    {
        public const string NotAuthorized = "lobby.not_authorized";
        public const string CannotRemoveLastAdmin = "lobby.cannot_remove_last_admin";
        public const string ConfigMissing = "lobby.config_missing";
        public const string InvalidPool = "lobby.invalid_pool";
        public const string InvalidSubject = "lobby.invalid_subject";
        public const string InvalidTicketId = "lobby.invalid_ticket_id";
        public const string InvalidWinnerSubject = "lobby.invalid_winner_subject";
        public const string InvalidAttributesJson = "lobby.invalid_attributes_json";
        public const string InvalidMatchSize = "lobby.invalid_match_size";
        public const string InvalidTtlSeconds = "lobby.invalid_ttl_seconds";
        public const string InvalidRating = "lobby.invalid_rating";
        public const string InvalidConfig = "lobby.invalid_config";
        public const string TicketNotFound = "lobby.ticket_not_found";
        public const string NotTicketOwner = "lobby.not_ticket_owner";
        public const string TicketNotQueued = "lobby.ticket_not_queued";
        public const string RoomNotFound = "lobby.room_not_found";
        public const string RoomClosed = "lobby.room_closed";
        public const string RoomNotActive = "lobby.room_not_active";
        public const string SeatNotFound = "lobby.seat_not_found";
        public const string SeatLeft = "lobby.seat_left";
        public const string ResultRequiresTwoSeats = "lobby.result_requires_two_seats";
        public const string WinnerNotInRoom = "lobby.winner_not_in_room";
    }

    // -----------------------------------------------------------------------
    // Tables
    // -----------------------------------------------------------------------

    [SpacetimeDB.Table(Accessor = "LobbyConfig")]
    public partial struct LobbyConfig
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public uint DefaultTicketTtlSeconds;
        public uint MaxMatchSize;
        /// <summary>Ready rooms whose seats aren't all joined by then are abandoned.</summary>
        public uint ReadyTimeoutSeconds;
        /// <summary>Finished tickets, rooms, seats and results are deleted after this.</summary>
        public uint RetentionSeconds;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "LobbySweepTick", Scheduled = nameof(LobbySweep), ScheduledAt = nameof(LobbySweepTick.ScheduledAt))]
    public partial struct LobbySweepTick
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
    }

    [SpacetimeDB.Table(Accessor = "LobbyAdminIdentity")]
    public partial struct LobbyAdminIdentity
    {
        [SpacetimeDB.PrimaryKey]
        public Identity Identity;
        public Timestamp AddedAt;
    }

    [SpacetimeDB.Table(Accessor = "LobbyQueueTicket")]
    [SpacetimeDB.Index.BTree(Accessor = "ByStatus", Columns = new[] { nameof(Status) })]
    [SpacetimeDB.Index.BTree(Accessor = "BySubject", Columns = new[] { nameof(Subject) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByCreatedAt", Columns = new[] { nameof(CreatedAt) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByStatusUpdatedAt", Columns = new[] { nameof(Status), nameof(UpdatedAt) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByPoolStatusCreatedAt", Columns = new[] { nameof(Pool), nameof(Status), nameof(CreatedAt) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByStatusExpiresAt", Columns = new[] { nameof(Status), nameof(ExpiresAt) })]
    [SpacetimeDB.Index.BTree(Accessor = "BySubjectStatus", Columns = new[] { nameof(Subject), nameof(Status) })]
    public partial struct LobbyQueueTicket
    {
        [SpacetimeDB.PrimaryKey]
        public string TicketId;
        public string Pool;
        public string Subject;
        public LobbyTicketStatus Status;
        public uint MatchSize;
        public bool Ranked;
        public int? Rating;
        public string? RatingPool;
        public string? PartyId;
        public string? AttributesJson;
        public ulong? RoomId;
        public Timestamp CreatedAt;
        public Timestamp UpdatedAt;
        public Timestamp ExpiresAt;
    }

    [SpacetimeDB.Table(Accessor = "LobbySubjectRating")]
    [SpacetimeDB.Index.BTree(Accessor = "ByPool", Columns = new[] { nameof(Pool) })]
    [SpacetimeDB.Index.BTree(Accessor = "BySubject", Columns = new[] { nameof(Subject) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByRating", Columns = new[] { nameof(Rating) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByLeaderboardOrder", Columns = new[] { nameof(Pool), nameof(RatingOrder), nameof(Subject) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByUpdatedAt", Columns = new[] { nameof(UpdatedAt) })]
    public partial struct LobbySubjectRating
    {
        [SpacetimeDB.PrimaryKey]
        public string RatingId;
        public string Pool;
        public string Subject;
        public int Rating;
        public long RatingOrder;
        public uint Wins;
        public uint Losses;
        public uint Draws;
        public uint Matches;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "LobbyRoom")]
    [SpacetimeDB.Index.BTree(Accessor = "ByStatus", Columns = new[] { nameof(Status) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByPool", Columns = new[] { nameof(Pool) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByCreatedAt", Columns = new[] { nameof(CreatedAt) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByStatusUpdatedAt", Columns = new[] { nameof(Status), nameof(UpdatedAt) })]
    public partial struct LobbyRoom
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong RoomId;
        public string Pool;
        public LobbyRoomStatus Status;
        public uint Capacity;
        public string? MetadataJson;
        public Timestamp CreatedAt;
        public Timestamp UpdatedAt;
        public Timestamp? ClosedAt;
    }

    [SpacetimeDB.Table(Accessor = "LobbyMatchResult")]
    [SpacetimeDB.Index.BTree(Accessor = "ByRoom", Columns = new[] { nameof(RoomId) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByPool", Columns = new[] { nameof(Pool) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByReportedAt", Columns = new[] { nameof(ReportedAt) })]
    public partial struct LobbyMatchResult
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ResultId;
        public ulong RoomId;
        public string Pool;
        public string? WinnerSubject;
        public string? LoserSubject;
        public string SubjectA;
        public string SubjectB;
        public int RatingABefore;
        public int RatingAAfter;
        public int RatingBBefore;
        public int RatingBAfter;
        public Timestamp ReportedAt;
    }

    [SpacetimeDB.Table(Accessor = "LobbyRoomSeat")]
    [SpacetimeDB.Index.BTree(Accessor = "ByRoom", Columns = new[] { nameof(RoomId) })]
    [SpacetimeDB.Index.BTree(Accessor = "BySubject", Columns = new[] { nameof(Subject) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByStatus", Columns = new[] { nameof(Status) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByRoomSubject", Columns = new[] { nameof(RoomId), nameof(Subject) })]
    public partial struct LobbyRoomSeat
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong SeatId;
        public ulong RoomId;
        public string Subject;
        public string? TicketId;
        public uint SeatIndex;
        public LobbySeatStatus Status;
        public Timestamp? JoinedAt;
        public Timestamp? LeftAt;
        public Timestamp UpdatedAt;
    }

    // -----------------------------------------------------------------------
    // Keys and matchmaking (src/keys.ts, src/matchmaking.ts)
    // -----------------------------------------------------------------------

    /// <summary>Each part length-prefixed (UTF-16 lengths, as in TS), so delimiters can't collide.</summary>
    public static string LobbyCompositeKey(params string[] parts) => string.Concat(parts.Select(p => $"{p.Length}:{p}"));

    /// <summary>How far from the anchor's rating a ranked match may reach: 100, plus 50 per 10 s waited, up to 800.</summary>
    public static long LobbyRankedBand(Timestamp createdAt, long nowMicros)
    {
        var created = createdAt.MicrosecondsSinceUnixEpoch;
        var waited = created >= nowMicros ? 0 : (nowMicros - created) / 1_000_000;
        return Math.Min(LobbyRankedMaxBand, LobbyRankedInitialBand + waited / LobbyRankedBandStepSeconds * LobbyRankedBandStep);
    }

    static long LobbyRatingOf(LobbyQueueTicket t) => t.Rating ?? LobbyDefaultRating;

    /// <summary>
    /// Picks matchSize tickets for the oldest anchor that can fill a match inside its rating
    /// band, taking the closest ratings first. queued is oldest first; returns indexes into it.
    /// </summary>
    public static List<int>? LobbyRankedSelection(IReadOnlyList<LobbyQueueTicket> queued, int matchSize, long nowMicros)
    {
        bool IsOlder(int a, int b) => queued[a].CreatedAt.MicrosecondsSinceUnixEpoch < queued[b].CreatedAt.MicrosecondsSinceUnixEpoch;
        // Pools in first-seen order, each sorted by rating then age (a stable sort, as in JS).
        var pools = new List<(string? Pool, List<int> Members)>();
        for (var i = 0; i < queued.Count; i++)
        {
            var at = pools.FindIndex(p => p.Pool == queued[i].RatingPool);
            if (at >= 0) pools[at].Members.Add(i);
            else pools.Add((queued[i].RatingPool, [i]));
        }
        var position = new int[queued.Count];
        var poolOf = new int[queued.Count];
        for (var pi = 0; pi < pools.Count; pi++)
        {
            var sorted = pools[pi].Members
                .OrderBy(x => x, Comparer<int>.Create((a, b) =>
                {
                    var c = LobbyRatingOf(queued[a]).CompareTo(LobbyRatingOf(queued[b]));
                    return c != 0 ? c : IsOlder(a, b) ? -1 : IsOlder(b, a) ? 1 : 0;
                }))
                .ToList();
            pools[pi] = (pools[pi].Pool, sorted);
            for (var idx = 0; idx < sorted.Count; idx++)
            {
                position[sorted[idx]] = idx;
                poolOf[sorted[idx]] = pi;
            }
        }
        for (var anchor = 0; anchor < queued.Count; anchor++)
        {
            var pool = pools[poolOf[anchor]].Members;
            if (pool.Count < matchSize) continue;
            var anchorRating = LobbyRatingOf(queued[anchor]);
            var band = LobbyRankedBand(queued[anchor].CreatedAt, nowMicros);
            var picked = new List<int> { anchor };
            var low = position[anchor] - 1;
            var high = position[anchor] + 1;
            while (picked.Count < matchSize)
            {
                int? left = low >= 0 ? pool[low] : null;
                int? right = high < pool.Count ? pool[high] : null;
                var leftGap = left is { } l ? anchorRating - LobbyRatingOf(queued[l]) : long.MaxValue;
                var rightGap = right is { } r ? LobbyRatingOf(queued[r]) - anchorRating : long.MaxValue;
                if (Math.Min(leftGap, rightGap) > band) break;
                if (leftGap < rightGap || (leftGap == rightGap && IsOlder(left!.Value, right!.Value)))
                {
                    picked.Add(left!.Value);
                    low--;
                }
                else
                {
                    picked.Add(right!.Value);
                    high++;
                }
            }
            if (picked.Count == matchSize) return picked;
        }
        return null;
    }

    public static double LobbyExpectedScore(int rating, int opponentRating) => 1 / (1 + Math.Pow(10, (opponentRating - rating) / 400.0));

    /// <summary>Math.round (half up), clamped to 100..5000.</summary>
    public static int LobbyUpdatedRating(int rating, double expected, double score) =>
        (int)Math.Clamp((long)Math.Floor(rating + LobbyEloK * (score - expected) + 0.5), LobbyMinRating, LobbyMaxRating);

    // -----------------------------------------------------------------------
    // Operations (src/submodule/operations.ts)
    // -----------------------------------------------------------------------

    /// <summary>JavaScript's String.prototype.trim whitespace.</summary>
    static string LobbyJsTrim(string s)
    {
        static bool Ws(char c) => c is (>= '\u0009' and <= '\u000d') or ' ' or '\u00a0' or '\u1680' or (>= '\u2000' and <= '\u200a') or '\u2028' or '\u2029' or '\u202f' or '\u205f' or '\u3000' or '\ufeff';
        int a = 0, b = s.Length;
        while (a < b && Ws(s[a])) a++;
        while (b > a && Ws(s[b - 1])) b--;
        return s[a..b];
    }

    static string LobbyNormalizeName(string value, string code, int max)
    {
        var o = LobbyJsTrim(value);
        if (o.Length == 0 || o.Length > max) throw new Exception(code);
        return o;
    }

    /// <summary>A JSON object (not null, not an array); returned trimmed but otherwise as given.</summary>
    static string? LobbyValidateAttributesJson(string? value)
    {
        if (value is null) return null;
        var o = LobbyJsTrim(value);
        if (o.Length == 0) return null;
        if (o.Length > LobbyMaxJsonLength || !LobbyJson.IsObject(o)) throw new Exception(LobbyErrors.InvalidAttributesJson);
        return o;
    }

    static string LobbyRatingId(string pool, string subject) => LobbyCompositeKey(pool, subject);

    static LobbySubjectRating LobbyGetOrCreateRating(LobbyTx tx, string pool, string subject) =>
        tx.Db.LobbySubjectRating.RatingId.Find(LobbyRatingId(pool, subject)) ?? tx.Db.LobbySubjectRating.Insert(new LobbySubjectRating
        {
            RatingId = LobbyRatingId(pool, subject),
            Pool = pool,
            Subject = subject,
            Rating = LobbyDefaultRating,
            RatingOrder = -LobbyDefaultRating,
            Wins = 0,
            Losses = 0,
            Draws = 0,
            Matches = 0,
            UpdatedAt = tx.Timestamp,
        });

    static LobbyConfig LobbyGetConfig(Local db) => db.LobbyConfig.Singleton.Find(true) ?? throw new Exception(LobbyErrors.ConfigMissing);

    public static bool LobbyIsAdmin(Local db, Identity identity) => db.LobbyAdminIdentity.Identity.Find(identity) is not null;

    /// <summary>Throws lobby.not_authorized unless the caller is a lobby admin.</summary>
    public static void LobbyRequireAdmin(Local db, Identity sender)
    {
        if (!LobbyIsAdmin(db, sender)) throw new Exception(LobbyErrors.NotAuthorized);
    }

    static bool LobbyIsTerminal(LobbyRoom room) => room.Status is LobbyRoomStatus.Closed or LobbyRoomStatus.Abandoned;

    static List<LobbyRoomSeat> LobbySeatsForRoom(Local db, ulong roomId) => db.LobbyRoomSeat.ByRoom.Filter(roomId).Take((int)LobbyMaxMatchSize).ToList();

    static LobbyRoomSeat? LobbyFindSeat(Local db, ulong roomId, string subject)
    {
        foreach (var seat in db.LobbyRoomSeat.ByRoomSubject.Filter((roomId, subject))) return seat;
        return null;
    }

    static void LobbyFinishRoom(LobbyTx tx, LobbyRoom room, LobbyRoomStatus status) =>
        tx.Db.LobbyRoom.RoomId.Update(room with { Status = status, UpdatedAt = tx.Timestamp, ClosedAt = tx.Timestamp });

    static ulong? LobbyAttemptMatch(LobbyTx tx, string pool, uint matchSize, bool ranked)
    {
        var now = tx.Timestamp.MicrosecondsSinceUnixEpoch;
        var queued = tx.Db.LobbyQueueTicket.ByPoolStatusCreatedAt.Filter((pool, LobbyTicketStatus.Queued))
            .Take(LobbyMaxMatchCandidates)
            .Where(t => t.Ranked == ranked && t.MatchSize == matchSize && t.ExpiresAt.MicrosecondsSinceUnixEpoch > now)
            .ToList();
        if (queued.Count < matchSize) return null;
        var selected = ranked ? LobbyRankedSelection(queued, (int)matchSize, now) : Enumerable.Range(0, (int)matchSize).ToList();
        if (selected is null) return null;
        var room = tx.Db.LobbyRoom.Insert(new LobbyRoom
        {
            RoomId = 0,
            Pool = pool,
            Status = LobbyRoomStatus.Ready,
            Capacity = matchSize,
            MetadataJson = ranked ? $"{{\"ranked\":true,\"ratingPool\":{LobbyJson.Quote(queued[selected[0]].RatingPool ?? pool)}}}" : null,
            CreatedAt = tx.Timestamp,
            UpdatedAt = tx.Timestamp,
            ClosedAt = null,
        });
        for (var index = 0; index < selected.Count; index++)
        {
            var ticket = queued[selected[index]];
            tx.Db.LobbyRoomSeat.Insert(new LobbyRoomSeat
            {
                SeatId = 0,
                RoomId = room.RoomId,
                Subject = ticket.Subject,
                TicketId = ticket.TicketId,
                SeatIndex = (uint)index,
                Status = LobbySeatStatus.Reserved,
                JoinedAt = null,
                LeftAt = null,
                UpdatedAt = tx.Timestamp,
            });
            tx.Db.LobbyQueueTicket.TicketId.Update(ticket with { Status = LobbyTicketStatus.Matched, RoomId = room.RoomId, UpdatedAt = tx.Timestamp });
        }
        return room.RoomId;
    }

    static LobbyJoinQueueResult LobbyEnqueue(LobbyTx tx, string poolArg, string subjectArg, uint matchSize, string? attributesJsonArg, uint? ttlSeconds, string? rankedPool)
    {
        var config = LobbyGetConfig(tx.Db);
        var pool = LobbyNormalizeName(poolArg, LobbyErrors.InvalidPool, LobbyMaxPoolLength);
        var subject = LobbyNormalizeName(subjectArg, LobbyErrors.InvalidSubject, LobbyMaxSubjectLength);
        if (matchSize < 1 || matchSize > config.MaxMatchSize) throw new Exception(LobbyErrors.InvalidMatchSize);
        var ttl = ttlSeconds ?? config.DefaultTicketTtlSeconds;
        if (ttl < 1 || ttl > LobbyMaxTtlSeconds) throw new Exception(LobbyErrors.InvalidTtlSeconds);
        var attributesJson = LobbyValidateAttributesJson(attributesJsonArg);
        var ratingPool = rankedPool is null ? null : LobbyNormalizeName(rankedPool, LobbyErrors.InvalidPool, LobbyMaxPoolLength);

        // A subject waits in one queue at a time.
        foreach (var t in tx.Db.LobbyQueueTicket.BySubjectStatus.Filter((subject, LobbyTicketStatus.Queued)).ToList())
        {
            tx.Db.LobbyQueueTicket.TicketId.Update(t with { Status = LobbyTicketStatus.Cancelled, UpdatedAt = tx.Timestamp });
        }

        var ticketId = $"ticket:{tx.NewUuidV7()}";
        int? rating = ratingPool is null ? null : tx.Db.LobbySubjectRating.RatingId.Find(LobbyRatingId(ratingPool, subject))?.Rating ?? LobbyDefaultRating;
        tx.Db.LobbyQueueTicket.Insert(new LobbyQueueTicket
        {
            TicketId = ticketId,
            Pool = pool,
            Subject = subject,
            Status = LobbyTicketStatus.Queued,
            MatchSize = matchSize,
            Ranked = rankedPool is not null,
            Rating = rating,
            RatingPool = ratingPool,
            PartyId = null,
            AttributesJson = attributesJson,
            RoomId = null,
            CreatedAt = tx.Timestamp,
            UpdatedAt = tx.Timestamp,
            ExpiresAt = new Timestamp(tx.Timestamp.MicrosecondsSinceUnixEpoch + (long)ttl * 1_000_000),
        });
        return new LobbyJoinQueueResult { TicketId = ticketId, RoomId = LobbyAttemptMatch(tx, pool, matchSize, rankedPool is not null) };
    }

    /// <summary>Queues Subject for an unranked match, replacing its queued ticket.</summary>
    public static LobbyJoinQueueResult LobbyJoinQueueForSubject(LobbyTx tx, JoinQueueArgs args) =>
        LobbyEnqueue(tx, args.Pool, args.Subject, args.MatchSize, args.AttributesJson, args.TtlSeconds, null);

    /// <summary>Queues Subject for a ranked match. Rating pools appear on the public leaderboard, so only the host chooses them.</summary>
    public static LobbyJoinQueueResult LobbyJoinRankedQueueForSubject(LobbyTx tx, JoinRankedQueueArgs args) =>
        LobbyEnqueue(tx, args.Pool, args.Subject, args.MatchSize, args.AttributesJson, args.TtlSeconds, args.RatingPool ?? args.Pool);

    /// <summary>The rating pool a ranked room recorded, or its pool.</summary>
    static string LobbyRatingPoolForRoom(LobbyRoom room)
    {
        var p = room.MetadataJson is { } m ? LobbyJson.RatingPool(m) : null;
        var trimmed = p is null ? "" : LobbyJsTrim(p);
        return trimmed.Length > 0 ? trimmed : room.Pool;
    }

    static LobbySubjectRating LobbyApplyResultRow(LobbyTx tx, LobbySubjectRating row, double score, int opponentRating)
    {
        var nextRating = LobbyUpdatedRating(row.Rating, LobbyExpectedScore(row.Rating, opponentRating), score);
        var next = row with
        {
            Rating = nextRating,
            RatingOrder = -nextRating,
            Wins = row.Wins + (score == 1 ? 1u : 0u),
            Losses = row.Losses + (score == 0 ? 1u : 0u),
            Draws = row.Draws + (score == 0.5 ? 1u : 0u),
            Matches = row.Matches + 1,
            UpdatedAt = tx.Timestamp,
        };
        tx.Db.LobbySubjectRating.RatingId.Update(next);
        return next;
    }

    /// <summary>
    /// Records the result of an active two-player room (null winner = a draw), updates both Elo
    /// ratings, and closes the room. Seats that left still get the result.
    /// </summary>
    public static void LobbyReportMatchResult(LobbyTx tx, ulong roomId, string? winnerSubject)
    {
        var room = tx.Db.LobbyRoom.RoomId.Find(roomId) ?? throw new Exception(LobbyErrors.RoomNotFound);
        if (room.Status != LobbyRoomStatus.Active) throw new Exception(LobbyErrors.RoomNotActive);
        var seats = LobbySeatsForRoom(tx.Db, roomId);
        if (seats.Count != 2) throw new Exception(LobbyErrors.ResultRequiresTwoSeats);
        seats = seats.OrderBy(s => s.SeatIndex).ToList();
        string a = seats[0].Subject, b = seats[1].Subject;
        var winner = winnerSubject is null ? null : LobbyNormalizeName(winnerSubject, LobbyErrors.InvalidWinnerSubject, LobbyMaxSubjectLength);
        if (winner is not null && winner != a && winner != b) throw new Exception(LobbyErrors.WinnerNotInRoom);
        var scoreA = winner is null ? 0.5 : winner == a ? 1.0 : 0.0;
        var ratingPool = LobbyRatingPoolForRoom(room);
        var ratingA = LobbyGetOrCreateRating(tx, ratingPool, a);
        var ratingB = LobbyGetOrCreateRating(tx, ratingPool, b);
        var nextA = LobbyApplyResultRow(tx, ratingA, scoreA, ratingB.Rating);
        var nextB = LobbyApplyResultRow(tx, ratingB, 1 - scoreA, ratingA.Rating);
        tx.Db.LobbyMatchResult.Insert(new LobbyMatchResult
        {
            ResultId = 0,
            RoomId = roomId,
            Pool = ratingPool,
            WinnerSubject = winner,
            LoserSubject = winner is null ? null : winner == a ? b : a,
            SubjectA = a,
            SubjectB = b,
            RatingABefore = ratingA.Rating,
            RatingAAfter = nextA.Rating,
            RatingBBefore = ratingB.Rating,
            RatingBAfter = nextB.Rating,
            ReportedAt = tx.Timestamp,
        });
        LobbyFinishRoom(tx, room, LobbyRoomStatus.Closed);
    }

    public static void LobbyCancelTicketForSubject(LobbyTx tx, string ticketId, string subject)
    {
        var id = LobbyNormalizeName(ticketId, LobbyErrors.InvalidTicketId, LobbyMaxTicketIdLength);
        var ticket = tx.Db.LobbyQueueTicket.TicketId.Find(id) ?? throw new Exception(LobbyErrors.TicketNotFound);
        if (ticket.Subject != subject) throw new Exception(LobbyErrors.NotTicketOwner);
        if (ticket.Status != LobbyTicketStatus.Queued) throw new Exception(LobbyErrors.TicketNotQueued);
        tx.Db.LobbyQueueTicket.TicketId.Update(ticket with { Status = LobbyTicketStatus.Cancelled, UpdatedAt = tx.Timestamp });
    }

    /// <summary>Marks Subject's reserved seat joined. The room activates once every seat joins.</summary>
    public static void LobbyJoinRoomForSubject(LobbyTx tx, ulong roomId, string subject)
    {
        var room = tx.Db.LobbyRoom.RoomId.Find(roomId) ?? throw new Exception(LobbyErrors.RoomNotFound);
        if (LobbyIsTerminal(room)) throw new Exception(LobbyErrors.RoomClosed);
        var seat = LobbyFindSeat(tx.Db, roomId, subject) ?? throw new Exception(LobbyErrors.SeatNotFound);
        if (seat.Status == LobbySeatStatus.Left) throw new Exception(LobbyErrors.SeatLeft);
        if (seat.Status == LobbySeatStatus.Joined) return;
        tx.Db.LobbyRoomSeat.SeatId.Update(seat with { Status = LobbySeatStatus.Joined, JoinedAt = tx.Timestamp, UpdatedAt = tx.Timestamp });
        if (room.Status == LobbyRoomStatus.Ready && LobbySeatsForRoom(tx.Db, roomId).All(s => s.Status == LobbySeatStatus.Joined))
        {
            tx.Db.LobbyRoom.RoomId.Update(room with { Status = LobbyRoomStatus.Active, UpdatedAt = tx.Timestamp });
        }
    }

    /// <summary>Marks Subject's seat left. Leaving a ready room abandons it; an active room stays active until every seat has left.</summary>
    public static void LobbyLeaveRoomForSubject(LobbyTx tx, ulong roomId, string subject)
    {
        var room = tx.Db.LobbyRoom.RoomId.Find(roomId) ?? throw new Exception(LobbyErrors.RoomNotFound);
        var seat = LobbyFindSeat(tx.Db, roomId, subject) ?? throw new Exception(LobbyErrors.SeatNotFound);
        if (seat.Status == LobbySeatStatus.Left || LobbyIsTerminal(room)) return;
        tx.Db.LobbyRoomSeat.SeatId.Update(seat with { Status = LobbySeatStatus.Left, LeftAt = tx.Timestamp, UpdatedAt = tx.Timestamp });
        if (room.Status == LobbyRoomStatus.Ready || LobbySeatsForRoom(tx.Db, roomId).All(s => s.Status == LobbySeatStatus.Left))
        {
            LobbyFinishRoom(tx, room, LobbyRoomStatus.Abandoned);
        }
    }

    /// <summary>Closes a room without a result. Use LobbyReportMatchResult to finish a ranked match.</summary>
    public static void LobbyCloseRoom(LobbyTx tx, ulong roomId)
    {
        var room = tx.Db.LobbyRoom.RoomId.Find(roomId) ?? throw new Exception(LobbyErrors.RoomNotFound);
        if (!LobbyIsTerminal(room)) LobbyFinishRoom(tx, room, LobbyRoomStatus.Closed);
    }

    static readonly Timestamp LobbyTsMin = new(long.MinValue);
    static readonly Timestamp LobbyTsMax = new(long.MaxValue);

    static Timestamp LobbyOlderThan(LobbyTx tx, uint seconds) => new(tx.Timestamp.MicrosecondsSinceUnixEpoch - (long)seconds * 1_000_000);

    static void LobbyDeleteRoom(LobbyTx tx, LobbyRoom room)
    {
        foreach (var seat in LobbySeatsForRoom(tx.Db, room.RoomId))
        {
            if (seat.TicketId is { } t) tx.Db.LobbyQueueTicket.TicketId.Delete(t);
            tx.Db.LobbyRoomSeat.SeatId.Delete(seat.SeatId);
        }
        tx.Db.LobbyRoom.RoomId.Delete(room.RoomId);
    }

    /// <summary>One bounded cleanup pass (runs on the schedule LobbyInstall creates; public for hosts and tests).</summary>
    public static void LobbySweepNow(LobbyTx tx)
    {
        var config = LobbyGetConfig(tx.Db);
        foreach (var t in tx.Db.LobbyQueueTicket.ByStatusExpiresAt.Filter((LobbyTicketStatus.Queued, new Bound<Timestamp>(LobbyTsMin, tx.Timestamp))).Take(LobbySweepBatch).ToList())
        {
            tx.Db.LobbyQueueTicket.TicketId.Update(t with { Status = LobbyTicketStatus.Expired, UpdatedAt = tx.Timestamp });
        }
        var retained = LobbyOlderThan(tx, config.RetentionSeconds);
        foreach (var status in new[] { LobbyTicketStatus.Cancelled, LobbyTicketStatus.Expired })
        {
            foreach (var t in tx.Db.LobbyQueueTicket.ByStatusUpdatedAt.Filter((status, new Bound<Timestamp>(LobbyTsMin, retained))).Take(LobbySweepBatch).ToList())
            {
                tx.Db.LobbyQueueTicket.TicketId.Delete(t.TicketId);
            }
        }
        foreach (var room in tx.Db.LobbyRoom.ByStatusUpdatedAt.Filter((LobbyRoomStatus.Ready, new Bound<Timestamp>(LobbyTsMin, LobbyOlderThan(tx, config.ReadyTimeoutSeconds)))).Take(LobbySweepBatch).ToList())
        {
            LobbyFinishRoom(tx, room, LobbyRoomStatus.Abandoned);
        }
        foreach (var status in new[] { LobbyRoomStatus.Closed, LobbyRoomStatus.Abandoned })
        {
            foreach (var room in tx.Db.LobbyRoom.ByStatusUpdatedAt.Filter((status, new Bound<Timestamp>(LobbyTsMin, retained))).Take(LobbySweepBatch).ToList())
            {
                LobbyDeleteRoom(tx, room);
            }
        }
        foreach (var r in tx.Db.LobbyMatchResult.ByReportedAt.Filter(new Bound<Timestamp>(LobbyTsMin, retained)).Take(LobbySweepBatch).ToList())
        {
            tx.Db.LobbyMatchResult.ResultId.Delete(r.ResultId);
        }
    }

    // -----------------------------------------------------------------------
    // Install, client reducers, procedure
    // -----------------------------------------------------------------------

    /// <summary>
    /// Call from your Init reducer: seeds the default config (60 s tickets, matches of up to 16,
    /// 120 s ready timeout, 1 h retention), makes the caller a lobby admin, and schedules the
    /// cleanup sweep every 15 s.
    /// </summary>
    public static void LobbyInstall(ReducerContext ctx)
    {
        if (ctx.Db.LobbyConfig.Singleton.Find(true) is null)
        {
            ctx.Db.LobbyConfig.Insert(new LobbyConfig
            {
                Singleton = true,
                DefaultTicketTtlSeconds = 60,
                MaxMatchSize = 16,
                ReadyTimeoutSeconds = 120,
                RetentionSeconds = 60 * 60,
                UpdatedAt = ctx.Timestamp,
            });
        }
        if (ctx.Db.LobbyAdminIdentity.Identity.Find(ctx.Sender) is null)
        {
            ctx.Db.LobbyAdminIdentity.Insert(new LobbyAdminIdentity { Identity = ctx.Sender, AddedAt = ctx.Timestamp });
        }
        if (ctx.Db.LobbySweepTick.Count == 0)
        {
            ctx.Db.LobbySweepTick.Insert(new LobbySweepTick { ScheduledId = 0, ScheduledAt = new ScheduleAt.Interval(new TimeDuration(LobbySweepIntervalMicros)) });
        }
    }

    /// <summary>The TS toHexString() of an identity: lowercase hex, no 0x.</summary>
    public static string LobbySubjectOf(Identity identity)
    {
        var s = identity.ToString();
        if (s.StartsWith("0x", StringComparison.OrdinalIgnoreCase)) s = s[2..];
        return s.ToLowerInvariant();
    }

    /// <summary>Queue the caller (their identity, as hex) for an unranked match. TS: join_queue.</summary>
    [SpacetimeDB.Reducer]
    public static void LobbyJoinQueue(ReducerContext ctx, string pool, uint matchSize, string? attributesJson, uint? ttlSeconds) =>
        LobbyJoinQueueForSubject(LobbyTx.Of(ctx), new JoinQueueArgs(pool, LobbySubjectOf(ctx.Sender), matchSize) { AttributesJson = attributesJson, TtlSeconds = ttlSeconds });

    /// <summary>TS: cancel_ticket.</summary>
    [SpacetimeDB.Reducer]
    public static void LobbyCancelTicket(ReducerContext ctx, string ticketId) => LobbyCancelTicketForSubject(LobbyTx.Of(ctx), ticketId, LobbySubjectOf(ctx.Sender));

    /// <summary>TS: join_room.</summary>
    [SpacetimeDB.Reducer]
    public static void LobbyJoinRoom(ReducerContext ctx, ulong roomId) => LobbyJoinRoomForSubject(LobbyTx.Of(ctx), roomId, LobbySubjectOf(ctx.Sender));

    /// <summary>TS: leave_room.</summary>
    [SpacetimeDB.Reducer]
    public static void LobbyLeaveRoom(ReducerContext ctx, ulong roomId) => LobbyLeaveRoomForSubject(LobbyTx.Of(ctx), roomId, LobbySubjectOf(ctx.Sender));

    /// <summary>Admins. TS: set_rating.</summary>
    [SpacetimeDB.Reducer]
    public static void LobbySetRating(ReducerContext ctx, string pool, string subject, int rating)
    {
        LobbyRequireAdmin(ctx.Db, ctx.Sender);
        var p = LobbyNormalizeName(pool, LobbyErrors.InvalidPool, LobbyMaxPoolLength);
        var s = LobbyNormalizeName(subject, LobbyErrors.InvalidSubject, LobbyMaxSubjectLength);
        if (rating < LobbyMinRating || rating > LobbyMaxRating) throw new Exception(LobbyErrors.InvalidRating);
        var existing = LobbyGetOrCreateRating(LobbyTx.Of(ctx), p, s);
        ctx.Db.LobbySubjectRating.RatingId.Update(existing with { Rating = rating, RatingOrder = -rating, UpdatedAt = ctx.Timestamp });
    }

    /// <summary>Admins. TS: update_config.</summary>
    [SpacetimeDB.Reducer]
    public static void LobbyUpdateConfig(ReducerContext ctx, uint defaultTicketTtlSeconds, uint maxMatchSize, uint readyTimeoutSeconds, uint retentionSeconds)
    {
        LobbyRequireAdmin(ctx.Db, ctx.Sender);
        if (defaultTicketTtlSeconds < 1 || defaultTicketTtlSeconds > LobbyMaxTtlSeconds || maxMatchSize < 1 || maxMatchSize > LobbyMaxMatchSize || readyTimeoutSeconds < 1 || retentionSeconds < 1)
        {
            throw new Exception(LobbyErrors.InvalidConfig);
        }
        var config = LobbyGetConfig(ctx.Db);
        ctx.Db.LobbyConfig.Singleton.Update(config with
        {
            DefaultTicketTtlSeconds = defaultTicketTtlSeconds,
            MaxMatchSize = maxMatchSize,
            ReadyTimeoutSeconds = readyTimeoutSeconds,
            RetentionSeconds = retentionSeconds,
            UpdatedAt = ctx.Timestamp,
        });
    }

    [SpacetimeDB.Reducer]
    public static void AddLobbyAdmin(ReducerContext ctx, Identity identity)
    {
        LobbyRequireAdmin(ctx.Db, ctx.Sender);
        if (ctx.Db.LobbyAdminIdentity.Identity.Find(identity) is null) ctx.Db.LobbyAdminIdentity.Insert(new LobbyAdminIdentity { Identity = identity, AddedAt = ctx.Timestamp });
    }

    [SpacetimeDB.Reducer]
    public static void RemoveLobbyAdmin(ReducerContext ctx, Identity identity)
    {
        LobbyRequireAdmin(ctx.Db, ctx.Sender);
        if (ctx.Db.LobbyAdminIdentity.Identity.Find(identity) is null) return;
        if (ctx.Db.LobbyAdminIdentity.Count <= 1) throw new Exception(LobbyErrors.CannotRemoveLastAdmin);
        ctx.Db.LobbyAdminIdentity.Identity.Delete(identity);
    }

    /// <summary>Every 15 s: expire tickets, abandon ready rooms past the timeout, delete what's past retention.</summary>
    [SpacetimeDB.Reducer]
    public static void LobbySweep(ReducerContext ctx, LobbySweepTick tick)
    {
        if (ctx.Sender != ctx.DatabaseIdentity) throw new Exception(LobbyErrors.NotAuthorized);
        LobbySweepNow(LobbyTx.Of(ctx));
    }

    [SpacetimeDB.Procedure]
    public static Result<LobbyStatus, string> GetLobbyStatus(ProcedureContext ctx)
    {
        try
        {
            return Result<LobbyStatus, string>.Ok(ctx.WithTx(tx =>
            {
                var config = LobbyGetConfig(tx.Db);
                return new LobbyStatus
                {
                    DefaultTicketTtlSeconds = config.DefaultTicketTtlSeconds,
                    MaxMatchSize = config.MaxMatchSize,
                    QueuedTickets = (uint)tx.Db.LobbyQueueTicket.ByStatus.Filter(LobbyTicketStatus.Queued).Take(LobbyMaxMatchCandidates).Count(),
                    ReadyRooms = (uint)tx.Db.LobbyRoom.ByStatus.Filter(LobbyRoomStatus.Ready).Take(LobbyMaxMatchCandidates).Count(),
                    ActiveRooms = (uint)tx.Db.LobbyRoom.ByStatus.Filter(LobbyRoomStatus.Active).Take(LobbyMaxMatchCandidates).Count(),
                };
            }));
        }
        catch (Exception e)
        {
            return Result<LobbyStatus, string>.Err(e.Message);
        }
    }

    // -----------------------------------------------------------------------
    // Views (src/submodule/views.ts)
    // -----------------------------------------------------------------------

    /// <summary>Insert each row after any equal ones and keep the first limit (TS selectTopRows).</summary>
    static List<T> LobbySelectTopRows<T>(IEnumerable<T> rows, int limit, Comparison<T> cmp)
    {
        var selected = new List<T>();
        foreach (var row in rows)
        {
            int low = 0, high = selected.Count;
            while (low < high)
            {
                var mid = (low + high) >>> 1;
                if (cmp(row, selected[mid]) < 0) high = mid;
                else low = mid + 1;
            }
            if (low >= limit) continue;
            selected.Insert(low, row);
            if (selected.Count > limit) selected.RemoveAt(selected.Count - 1);
        }
        return selected;
    }

    static int LobbyNewestFirst(Timestamp a, Timestamp b) => b.MicrosecondsSinceUnixEpoch.CompareTo(a.MicrosecondsSinceUnixEpoch);

    static LobbyRankedRatingRow LobbyRatingRow(LobbySubjectRating r) => new()
    {
        Pool = r.Pool,
        Subject = r.Subject,
        Rating = r.Rating,
        Wins = r.Wins,
        Losses = r.Losses,
        Draws = r.Draws,
        Matches = r.Matches,
    };

    [SpacetimeDB.View(Accessor = "MyLobbyTickets", Public = true)]
    public static List<LobbyQueueTicket> MyLobbyTickets(ViewContext ctx) =>
        LobbySelectTopRows(ctx.Db.LobbyQueueTicket.BySubject.Filter(LobbySubjectOf(ctx.Sender)), LobbyMaxViewRows, (a, b) => LobbyNewestFirst(a.CreatedAt, b.CreatedAt));

    [SpacetimeDB.View(Accessor = "MyLobbyRatings", Public = true)]
    public static List<LobbyRankedRatingRow> MyLobbyRatings(ViewContext ctx) =>
        LobbySelectTopRows(ctx.Db.LobbySubjectRating.BySubject.Filter(LobbySubjectOf(ctx.Sender)), LobbyMaxViewRows, (a, b) => string.CompareOrdinal(a.Pool, b.Pool))
            .Select(LobbyRatingRow)
            .ToList();

    [SpacetimeDB.View(Accessor = "MyLobbyRoomSeats", Public = true)]
    public static List<LobbyRoomSeat> MyLobbyRoomSeats(ViewContext ctx) =>
        LobbySelectTopRows(ctx.Db.LobbyRoomSeat.BySubject.Filter(LobbySubjectOf(ctx.Sender)), LobbyMaxViewRows, (a, b) => LobbyNewestFirst(a.UpdatedAt, b.UpdatedAt));

    [SpacetimeDB.View(Accessor = "MyLobbyRooms", Public = true)]
    public static List<LobbyRoom> MyLobbyRooms(ViewContext ctx)
    {
        var seats = LobbySelectTopRows(ctx.Db.LobbyRoomSeat.BySubject.Filter(LobbySubjectOf(ctx.Sender)), LobbyMaxViewRows, (a, b) => LobbyNewestFirst(a.UpdatedAt, b.UpdatedAt));
        var seen = new HashSet<ulong>();
        var rooms = new List<LobbyRoom>();
        foreach (var seat in seats)
        {
            if (!seen.Add(seat.RoomId)) continue;
            if (ctx.Db.LobbyRoom.RoomId.Find(seat.RoomId) is { } room) rooms.Add(room);
        }
        return rooms.OrderBy(r => r, Comparer<LobbyRoom>.Create((a, b) => LobbyNewestFirst(a.CreatedAt, b.CreatedAt))).ToList();
    }

    [SpacetimeDB.View(Accessor = "LobbyQueueSummary", Public = true)]
    public static List<LobbyQueueSummaryRow> LobbyQueueSummary(ViewContext ctx)
    {
        var rows = new List<LobbyQueueSummaryRow>();
        var index = new Dictionary<string, int>();
        int Ensure(string pool)
        {
            if (index.TryGetValue(pool, out var i)) return i;
            rows.Add(new LobbyQueueSummaryRow { Pool = pool });
            return index[pool] = rows.Count - 1;
        }
        foreach (var t in ctx.Db.LobbyQueueTicket.ByStatus.Filter(LobbyTicketStatus.Queued).Take(LobbyMaxMatchCandidates))
        {
            var i = Ensure(t.Pool);
            rows[i] = rows[i] with { QueuedTickets = rows[i].QueuedTickets + 1 };
        }
        foreach (var r in ctx.Db.LobbyRoom.ByStatus.Filter(LobbyRoomStatus.Ready).Take(LobbyMaxMatchCandidates))
        {
            var i = Ensure(r.Pool);
            rows[i] = rows[i] with { ReadyRooms = rows[i].ReadyRooms + 1 };
        }
        foreach (var r in ctx.Db.LobbyRoom.ByStatus.Filter(LobbyRoomStatus.Active).Take(LobbyMaxMatchCandidates))
        {
            var i = Ensure(r.Pool);
            rows[i] = rows[i] with { ActiveRooms = rows[i].ActiveRooms + 1 };
        }
        return rows.OrderBy(r => r.Pool, StringComparer.Ordinal).ToList();
    }

    [SpacetimeDB.View(Accessor = "LobbyRankedLeaderboard", Public = true)]
    public static List<LobbyRankedRatingRow> LobbyRankedLeaderboard(ViewContext ctx) =>
        // Walk the whole (Pool, RatingOrder, Subject) index: per pool, best first.
        ctx.Db.LobbySubjectRating.ByLeaderboardOrder.Filter(new Bound<string>("", "\uffff\uffff\uffff\uffff")).Take(500).Select(LobbyRatingRow).ToList();

    [SpacetimeDB.View(Accessor = "LobbyAdminTickets", Public = true)]
    public static List<LobbyQueueTicket> LobbyAdminTickets(ViewContext ctx) =>
        !LobbyIsAdmin(ctx) ? [] : LobbySelectTopRows(ctx.Db.LobbyQueueTicket.ByCreatedAt.Filter(new Bound<Timestamp>(LobbyTsMin, LobbyTsMax)), LobbyMaxViewRows, (a, b) => LobbyNewestFirst(a.CreatedAt, b.CreatedAt));

    [SpacetimeDB.View(Accessor = "LobbyAdminRooms", Public = true)]
    public static List<LobbyRoom> LobbyAdminRooms(ViewContext ctx) =>
        !LobbyIsAdmin(ctx) ? [] : LobbySelectTopRows(ctx.Db.LobbyRoom.ByCreatedAt.Filter(new Bound<Timestamp>(LobbyTsMin, LobbyTsMax)), LobbyMaxViewRows, (a, b) => LobbyNewestFirst(a.CreatedAt, b.CreatedAt));

    [SpacetimeDB.View(Accessor = "LobbyAdminRoomSeats", Public = true)]
    public static List<LobbyRoomSeat> LobbyAdminRoomSeats(ViewContext ctx) =>
        !LobbyIsAdmin(ctx) ? [] : ctx.Db.LobbyRoomSeat.ByRoom.Filter(new Bound<ulong>(0, ulong.MaxValue)).Take(1000).ToList();

    [SpacetimeDB.View(Accessor = "LobbyAdminMatchResults", Public = true)]
    public static List<LobbyMatchResult> LobbyAdminMatchResults(ViewContext ctx) =>
        !LobbyIsAdmin(ctx) ? [] : LobbySelectTopRows(ctx.Db.LobbyMatchResult.ByReportedAt.Filter(new Bound<Timestamp>(LobbyTsMin, LobbyTsMax)), 500, (a, b) => LobbyNewestFirst(a.ReportedAt, b.ReportedAt));

    static bool LobbyIsAdmin(ViewContext ctx) => ctx.Db.LobbyAdminIdentity.Identity.Find(ctx.Sender) is not null;
}

/// <summary>JSON helpers: validate an object, quote a string, read the room metadata Lobby.cs writes.</summary>
static class LobbyJson
{
    /// <summary>JSON.stringify(s) for a string.</summary>
    public static string Quote(string s)
    {
        var sb = new StringBuilder("\"");
        foreach (var c in s)
        {
            switch (c)
            {
                case '"': sb.Append("\\\""); break;
                case '\\': sb.Append("\\\\"); break;
                case '\b': sb.Append("\\b"); break;
                case '\f': sb.Append("\\f"); break;
                case '\n': sb.Append("\\n"); break;
                case '\r': sb.Append("\\r"); break;
                case '\t': sb.Append("\\t"); break;
                default:
                    if (c < 0x20) sb.Append($"\\u{(int)c:x4}");
                    else sb.Append(c);
                    break;
            }
        }
        return sb.Append('"').ToString();
    }

    /// <summary>Whether text is valid JSON (JSON.parse rules) whose value is an object.</summary>
    public static bool IsObject(string text)
    {
        var i = 0;
        Ws(text, ref i);
        if (i >= text.Length || text[i] != '{' || !SkipValue(text, ref i, 0)) return false;
        Ws(text, ref i);
        return i == text.Length;
    }

    static void Ws(string s, ref int i)
    {
        while (i < s.Length && s[i] is ' ' or '\t' or '\n' or '\r') i++;
    }

    static bool SkipValue(string s, ref int i, int depth)
    {
        if (depth > 512 || i >= s.Length) return false;
        switch (s[i])
        {
            case '{':
            case '[':
            {
                var close = s[i] == '{' ? '}' : ']';
                var obj = close == '}';
                i++;
                Ws(s, ref i);
                if (i < s.Length && s[i] == close)
                {
                    i++;
                    return true;
                }
                while (true)
                {
                    Ws(s, ref i);
                    if (obj)
                    {
                        if (i >= s.Length || s[i] != '"' || !SkipString(s, ref i)) return false;
                        Ws(s, ref i);
                        if (i >= s.Length || s[i] != ':') return false;
                        i++;
                        Ws(s, ref i);
                    }
                    if (!SkipValue(s, ref i, depth + 1)) return false;
                    Ws(s, ref i);
                    if (i >= s.Length) return false;
                    if (s[i] == ',')
                    {
                        i++;
                        continue;
                    }
                    if (s[i] == close)
                    {
                        i++;
                        return true;
                    }
                    return false;
                }
            }
            case '"':
                return SkipString(s, ref i);
            case 't':
                return SkipLit(s, ref i, "true");
            case 'f':
                return SkipLit(s, ref i, "false");
            case 'n':
                return SkipLit(s, ref i, "null");
            default:
            {
                int Digits(ref int k)
                {
                    var st = k;
                    while (k < s.Length && s[k] is >= '0' and <= '9') k++;
                    return k - st;
                }
                if (s[i] == '-') i++;
                if (i < s.Length && s[i] == '0') i++;
                else if (Digits(ref i) == 0) return false;
                if (i < s.Length && s[i] == '.')
                {
                    i++;
                    if (Digits(ref i) == 0) return false;
                }
                if (i < s.Length && s[i] is 'e' or 'E')
                {
                    i++;
                    if (i < s.Length && s[i] is '+' or '-') i++;
                    if (Digits(ref i) == 0) return false;
                }
                return true;
            }
        }
    }

    static bool SkipLit(string s, ref int i, string word)
    {
        if (string.CompareOrdinal(s, i, word, 0, word.Length) != 0) return false;
        i += word.Length;
        return true;
    }

    static bool SkipString(string s, ref int i)
    {
        i++;
        while (i < s.Length)
        {
            var c = s[i];
            if (c == '"')
            {
                i++;
                return true;
            }
            if (c == '\\')
            {
                i++;
                if (i >= s.Length) return false;
                if (s[i] is '"' or '\\' or '/' or 'b' or 'f' or 'n' or 'r' or 't') i++;
                else if (s[i] == 'u' && i + 4 < s.Length && s.AsSpan(i + 1, 4).ToString().All(char.IsAsciiHexDigit)) i += 5;
                else return false;
                continue;
            }
            if (c < 0x20) return false;
            i++;
        }
        return false;
    }

    /// <summary>The ratingPool string in room metadata Lobby.cs wrote ({"ranked":true,"ratingPool":"..."}).</summary>
    public static string? RatingPool(string metadata)
    {
        const string prefix = "{\"ranked\":true,\"ratingPool\":\"";
        if (!metadata.StartsWith(prefix, StringComparison.Ordinal)) return null;
        var sb = new StringBuilder();
        for (var i = prefix.Length; i < metadata.Length; i++)
        {
            var c = metadata[i];
            if (c == '"') return metadata[(i + 1)..] == "}" ? sb.ToString() : null;
            if (c != '\\')
            {
                sb.Append(c);
                continue;
            }
            if (++i >= metadata.Length) return null;
            switch (metadata[i])
            {
                case 'n': sb.Append('\n'); break;
                case 'r': sb.Append('\r'); break;
                case 't': sb.Append('\t'); break;
                case 'b': sb.Append('\b'); break;
                case 'f': sb.Append('\f'); break;
                case 'u':
                    if (i + 4 >= metadata.Length) return null;
                    sb.Append((char)ushort.Parse(metadata.AsSpan(i + 1, 4), NumberStyles.HexNumber, CultureInfo.InvariantCulture));
                    i += 4;
                    break;
                default: sb.Append(metadata[i]); break;
            }
        }
        return null;
    }
}
