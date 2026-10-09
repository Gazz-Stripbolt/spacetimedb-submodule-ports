// Demo for RateLimit.cs: the upstream example (install + admin reducers) plus procedures the
// test suite uses to consume and peek with arbitrary policies. The C# twin of demo/rust.

#pragma warning disable STDB_UNSTABLE

using SpacetimeDB;

[SpacetimeDB.Type]
public partial struct DemoConsumeResult
{
    public bool Allowed;
    public string Key;
    public string Scope;
    public uint Limit;
    public uint Used;
    public uint Remaining;
    public Timestamp ResetAt;
    public uint RetryAfterSeconds;
}

[SpacetimeDB.Type]
public partial struct DemoPeekResult
{
    public uint Limit;
    public uint WindowSeconds;
    public uint Used;
    public uint Remaining;
    public Timestamp? ResetAt;
}

public static partial class Module
{
    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx) => RateLimitInstall(ctx);

    [SpacetimeDB.Procedure]
    public static Result<DemoConsumeResult, string> DemoConsume(ProcedureContext ctx, string scope, uint limit, uint windowSeconds, string key, uint cost)
    {
        try
        {
            var r = ctx.WithTx(tx => new RateLimitPolicy(scope, limit, windowSeconds).Consume(tx.Db, tx.Timestamp, key, cost));
            return Result<DemoConsumeResult, string>.Ok(new DemoConsumeResult
            {
                Allowed = r.Allowed,
                Key = r.Key,
                Scope = r.Scope,
                Limit = r.Limit,
                Used = r.Used,
                Remaining = r.Remaining,
                ResetAt = r.ResetAt,
                RetryAfterSeconds = r.RetryAfterSeconds,
            });
        }
        catch (Exception e)
        {
            return Result<DemoConsumeResult, string>.Err(e.Message);
        }
    }

    /// <summary>Peek, with now from the procedure's clock when useNow is set.</summary>
    [SpacetimeDB.Procedure]
    public static Result<DemoPeekResult, string> DemoPeek(ProcedureContext ctx, string scope, uint limit, uint windowSeconds, string key, bool useNow)
    {
        try
        {
            var s = ctx.WithTx(tx => new RateLimitPolicy(scope, limit, windowSeconds).Peek(tx.Db, key, useNow ? tx.Timestamp : null));
            return Result<DemoPeekResult, string>.Ok(new DemoPeekResult
            {
                Limit = s.Limit,
                WindowSeconds = s.WindowSeconds,
                Used = s.Used,
                Remaining = s.Remaining,
                ResetAt = s.ResetAt,
            });
        }
        catch (Exception e)
        {
            return Result<DemoPeekResult, string>.Err(e.Message);
        }
    }
}
