// Demo for ApiKeys.cs: the upstream example module (install + the client operations) plus
// procedures the test suite uses to verify keys and drive the host helpers. The C# twin of
// demo/rust.

#pragma warning disable STDB_UNSTABLE

using SpacetimeDB;

public static partial class Module
{
    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx) => ApiKeysInstall(ctx);

    /// <summary>What an HTTP handler or procedure receiving a key would do.</summary>
    [SpacetimeDB.Procedure]
    public static Result<ApiKeyVerifyResult, string> DemoVerify(ProcedureContext ctx, string key, string? requiredScope, string? action) =>
        ApiKeysTry(() => ctx.WithTx(tx => ApiKeysVerify(ApiKeysTx.Of(ctx, tx), new VerifyApiKeyArgs(key) { RequiredScope = requiredScope, Action = action })));

    [SpacetimeDB.Procedure]
    public static bool DemoHasScope(ProcedureContext ctx, string scopesJson, string? required) => ApiKeysHasScope(scopesJson, required);

    /// <summary>JSON.stringify(JSON.parse(text)), or the error.</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoJson(ProcedureContext ctx, string text)
    {
        var v = ApiKeysJson.Parse(text);
        return ReferenceEquals(v, ApiKeysJson.Invalid) ? Result<string, string>.Err("invalid") : Result<string, string>.Ok(ApiKeysJson.Stringify(v));
    }

    /// <summary>The derived key for a secret, counter and timestamp (cross-language vector check).</summary>
    [SpacetimeDB.Procedure]
    public static List<string> DemoDerive(ProcedureContext ctx, string secret, ulong counter, long micros, string keyPrefix)
    {
        var (key, prefix) = ApiKeysFormat(keyPrefix, ApiKeysDeriveSecret(secret, counter, micros));
        return [key, prefix, ApiKeysHash(key)];
    }

    /// <summary>Backdate a usage row, so the retention sweep has something to delete.</summary>
    [SpacetimeDB.Reducer]
    public static void DemoBackdateUsage(ReducerContext ctx, ulong usageId, uint seconds)
    {
        ApiKeysRequireAdmin(ctx.Db, ctx.Sender);
        var row = ctx.Db.ApiKeyUsage.UsageId.Find(usageId) ?? throw new Exception("no such usage row");
        var usedAt = new Timestamp(row.UsedAt.MicrosecondsSinceUnixEpoch - (long)seconds * 1_000_000);
        ctx.Db.ApiKeyUsage.UsageId.Update(row with { UsedAt = usedAt, UsedAtOrder = -usedAt.MicrosecondsSinceUnixEpoch });
    }

    [SpacetimeDB.Procedure]
    public static Result<uint, string> DemoRunUsageSweep(ProcedureContext ctx) =>
        ApiKeysTry(() => ctx.WithTx(tx =>
        {
            ApiKeysRequireAdmin(tx.Db, tx.Sender);
            return ApiKeysRunUsageSweep(tx.Db, tx.Timestamp);
        }));
}
