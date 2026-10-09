// Demo for Lobby.cs: the upstream example module (install + the client operations) plus
// procedures the test suite uses to act as any subject and to run the matchmaking helpers.
// The C# twin of demo/rust.

#pragma warning disable STDB_UNSTABLE

using SpacetimeDB;

[SpacetimeDB.Type]
public partial struct DemoTicket
{
    public string TicketId;
    public int? Rating;
    public string? RatingPool;
    public long CreatedAtMicros;
}

public static partial class Module
{
    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx) => LobbyInstall(ctx);

    static Result<T, string> DemoTry<T>(Func<T> f)
    {
        try
        {
            return Result<T, string>.Ok(f());
        }
        catch (Exception e)
        {
            return Result<T, string>.Err(e.Message);
        }
    }

    [SpacetimeDB.Procedure]
    public static Result<LobbyJoinQueueResult, string> DemoJoinQueue(ProcedureContext ctx, string pool, string subject, uint matchSize, string? attributesJson, uint? ttlSeconds) =>
        DemoTry(() => ctx.WithTx(tx => LobbyJoinQueueForSubject(LobbyTx.Of(ctx, tx), new JoinQueueArgs(pool, subject, matchSize) { AttributesJson = attributesJson, TtlSeconds = ttlSeconds })));

    [SpacetimeDB.Procedure]
    public static Result<LobbyJoinQueueResult, string> DemoJoinRankedQueue(ProcedureContext ctx, string pool, string subject, uint matchSize, string? attributesJson, uint? ttlSeconds, string? ratingPool) =>
        DemoTry(() => ctx.WithTx(tx => LobbyJoinRankedQueueForSubject(LobbyTx.Of(ctx, tx), new JoinRankedQueueArgs(pool, subject, matchSize) { AttributesJson = attributesJson, TtlSeconds = ttlSeconds, RatingPool = ratingPool })));

    [SpacetimeDB.Reducer]
    public static void DemoCancelTicket(ReducerContext ctx, string ticketId, string subject) => LobbyCancelTicketForSubject(LobbyTx.Of(ctx), ticketId, subject);

    [SpacetimeDB.Reducer]
    public static void DemoJoinRoom(ReducerContext ctx, ulong roomId, string subject) => LobbyJoinRoomForSubject(LobbyTx.Of(ctx), roomId, subject);

    [SpacetimeDB.Reducer]
    public static void DemoLeaveRoom(ReducerContext ctx, ulong roomId, string subject) => LobbyLeaveRoomForSubject(LobbyTx.Of(ctx), roomId, subject);

    [SpacetimeDB.Reducer]
    public static void DemoReportResult(ReducerContext ctx, ulong roomId, string? winnerSubject) => LobbyReportMatchResult(LobbyTx.Of(ctx), roomId, winnerSubject);

    [SpacetimeDB.Reducer]
    public static void DemoCloseRoom(ReducerContext ctx, ulong roomId) => LobbyCloseRoom(LobbyTx.Of(ctx), roomId);

    /// <summary>Run one sweep pass now (the scheduled one runs every 15 s).</summary>
    [SpacetimeDB.Reducer]
    public static void DemoSweep(ReducerContext ctx)
    {
        LobbyRequireAdmin(ctx.Db, ctx.Sender);
        LobbySweepNow(LobbyTx.Of(ctx));
    }

    /// <summary>rankedSelection over synthetic tickets; returns the picked ticket ids.</summary>
    [SpacetimeDB.Procedure]
    public static List<string>? DemoRankedSelection(ProcedureContext ctx, List<DemoTicket> tickets, uint matchSize, long nowMicros)
    {
        var queued = tickets.Select(t => new LobbyQueueTicket
        {
            TicketId = t.TicketId,
            Pool = "p",
            Subject = t.TicketId,
            Status = LobbyTicketStatus.Queued,
            MatchSize = matchSize,
            Ranked = true,
            Rating = t.Rating,
            RatingPool = t.RatingPool,
            CreatedAt = new Timestamp(t.CreatedAtMicros),
            UpdatedAt = new Timestamp(t.CreatedAtMicros),
            ExpiresAt = new Timestamp(long.MaxValue),
        }).ToList();
        return LobbyRankedSelection(queued, (int)matchSize, nowMicros)?.Select(i => queued[i].TicketId).ToList();
    }

    [SpacetimeDB.Procedure]
    public static List<double> DemoElo(ProcedureContext ctx, int rating, int opponent, double score)
    {
        var e = LobbyExpectedScore(rating, opponent);
        return [e, LobbyUpdatedRating(rating, e, score)];
    }

    [SpacetimeDB.Procedure]
    public static long DemoBand(ProcedureContext ctx, long createdAtMicros, long nowMicros) => LobbyRankedBand(new Timestamp(createdAtMicros), nowMicros);

    [SpacetimeDB.Procedure]
    public static string DemoKey(ProcedureContext ctx, List<string> parts) => LobbyCompositeKey(parts.ToArray());
}
