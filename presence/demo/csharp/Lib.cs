// Demo for Presence.cs: the upstream example module (install + the client operations) plus
// procedures the test suite uses to drive the host helpers in arbitrary scopes. The C# twin
// of demo/rust.

#pragma warning disable STDB_UNSTABLE

using SpacetimeDB;

public static partial class Module
{
    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx) => PresenceInstall(ctx);

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
    public static Result<PresenceEntry, string> DemoUpsert(ProcedureContext ctx, string scope, string subject, string? status, string? activity, string? payloadJson, uint? ttlSeconds) =>
        DemoTry(() => ctx.WithTx(tx => PresenceUpsert(tx.Db, tx.Timestamp, new PresenceUpsertOpts(scope, subject)
        {
            Status = status,
            Activity = activity,
            PayloadJson = payloadJson,
            TtlSeconds = ttlSeconds,
        })));

    [SpacetimeDB.Procedure]
    public static Result<PresenceEntry, string> DemoTouch(ProcedureContext ctx, string scope, string subject, uint? ttlSeconds) =>
        DemoTry(() => ctx.WithTx(tx => PresenceTouch(tx.Db, tx.Timestamp, scope, subject, ttlSeconds)));

    [SpacetimeDB.Procedure]
    public static Result<bool, string> DemoRemove(ProcedureContext ctx, string scope, string subject) =>
        DemoTry(() => ctx.WithTx(tx => PresenceRemove(tx.Db, scope, subject)));

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoKey(ProcedureContext ctx, string scope, string subject) =>
        DemoTry(() => PresenceBuildKey(scope, subject));
}
