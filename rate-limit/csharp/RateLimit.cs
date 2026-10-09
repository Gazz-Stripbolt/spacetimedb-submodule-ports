// RateLimit.cs: fixed-window rate limits for C# modules.
//
// Unofficial C# port of @spacetimedb/rate-limit (SpacetimeDB PR #5903, commit
// ae25f1da24b1723e28acd0038e75e598f33952c3, package spacetime-rate-limit-ts). Licensed
// Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
// is by Tinker (Gazz-Stripbolt).
//
// Same tables (rate_limit_bucket, rate_limit_admin_identity, rate_limit_config,
// rate_limit_sweep_tick), bucket keys, error codes and window semantics as the TS package.
//
//     static readonly RateLimitPolicy Login = new("auth.login", Limit: 5, WindowSeconds: 60);
//
//     [Reducer(ReducerKind.Init)]
//     public static void Init(ReducerContext ctx) => RateLimitInstall(ctx);
//
//     [Reducer]
//     public static void LogIn(ReducerContext ctx, string user)
//     {
//         var r = Login.Consume(ctx, ctx.Sender.ToString());
//         if (!r.Allowed) throw new Exception($"slow down, try again in {r.RetryAfterSeconds}s");
//         ...
//     }
//
// Changes from the TypeScript original:
// - A scope used with two different limits or windows fails on use with
//   rate_limit.duplicate_scope (TS rejects the second client() while the module loads), and
//   policy validation errors are reported on use too.
// - The generic reducer names are prefixed: update_config -> rate_limit_update_config,
//   reset_buckets -> rate_limit_reset_buckets, run_sweep -> rate_limit_run_sweep.
// - Errors are thrown as Exceptions carrying the same rate_limit.* codes.

#pragma warning disable STDB_UNSTABLE
#nullable enable

using SpacetimeDB;

/// <summary>The outcome of <see cref="RateLimitPolicy.Consume(ReducerContext, string, uint)"/>.</summary>
public readonly record struct RateLimitResult(
    bool Allowed,
    string Key,
    string Scope,
    uint Limit,
    uint Used,
    uint Remaining,
    Timestamp ResetAt,
    /// <summary>0 when allowed; otherwise whole seconds until the window resets (rounded up).</summary>
    uint RetryAfterSeconds);

/// <summary>Current usage of one actor's bucket. ResetAt is null when no window is open.</summary>
public readonly record struct RateLimitStatus(uint Limit, uint WindowSeconds, uint Used, uint Remaining, Timestamp? ResetAt);

/// <summary>One scope's policy: at most Limit units per WindowSeconds, per actor.</summary>
public readonly record struct RateLimitPolicy(string Scope, uint Limit, uint WindowSeconds)
{
    const int MaxScopeLength = 128;
    const int MaxActorKeyLength = 256;

    /// <summary>Scopes seen in this module instance, with their policies.</summary>
    static readonly Dictionary<string, (uint, uint)> Registered = new();

    void Check()
    {
        if (Scope.Length == 0 || Scope.Length > MaxScopeLength) throw new Exception(Module.RateLimitErrors.InvalidScope);
        if (Limit == 0) throw new Exception(Module.RateLimitErrors.InvalidLimit);
        if (WindowSeconds == 0) throw new Exception(Module.RateLimitErrors.InvalidWindow);
        if (Registered.TryGetValue(Scope, out var existing))
        {
            if (existing != (Limit, WindowSeconds)) throw new Exception(Module.RateLimitErrors.DuplicateScope);
        }
        else Registered[Scope] = (Limit, WindowSeconds);
    }

    /// <summary>The stored key: both parts length-prefixed (UTF-16 lengths, as in TS) so delimiters can't collide.</summary>
    public string BucketKey(string actorKey)
    {
        if (actorKey.Length == 0 || actorKey.Length > MaxActorKeyLength) throw new Exception(Module.RateLimitErrors.InvalidActorKey);
        return $"{Scope.Length}:{Scope}{actorKey.Length}:{actorKey}";
    }

    public RateLimitResult Consume(ReducerContext ctx, string actorKey, uint cost = 1) => Consume(ctx.Db, ctx.Timestamp, actorKey, cost);

    /// <summary>
    /// Spend <c>cost</c> from the actor's bucket in the caller's transaction (use this overload
    /// with a procedure's tx.Db and tx.Timestamp). A cost of 0 or above Limit can never be
    /// allowed and throws rate_limit.invalid_cost.
    /// </summary>
    public RateLimitResult Consume(Local db, Timestamp now, string actorKey, uint cost = 1)
    {
        Check();
        var key = BucketKey(actorKey);
        if (cost == 0 || cost > Limit) throw new Exception(Module.RateLimitErrors.InvalidCost);
        var existing = db.RateLimitBucket.Key.Find(key);
        var open = existing is { } e && e.ExpiresAt.MicrosecondsSinceUnixEpoch > now.MicrosecondsSinceUnixEpoch ? existing : null;
        var used = open?.Count ?? 0u;
        var resetAt = open?.ExpiresAt ?? new Timestamp(now.MicrosecondsSinceUnixEpoch + (long)WindowSeconds * 1_000_000);

        if ((ulong)used + cost > Limit)
        {
            return new RateLimitResult(false, key, Scope, Limit, used, 0, resetAt, SecondsUntil(now, resetAt));
        }
        var row = new Module.RateLimitBucket
        {
            Key = key,
            Scope = Scope,
            WindowStart = open?.WindowStart ?? now,
            ExpiresAt = resetAt,
            Count = used + cost,
            UpdatedAt = now,
        };
        // Update any existing row, including an expired one not yet swept.
        if (existing is not null) db.RateLimitBucket.Key.Update(row);
        else db.RateLimitBucket.Insert(row);
        return new RateLimitResult(true, key, Scope, Limit, used + cost, Limit - used - cost, resetAt, 0);
    }

    public RateLimitStatus Peek(ReducerContext ctx, string actorKey, Timestamp? now = null) =>
        Peek(k => ctx.Db.RateLimitBucket.Key.Find(k), actorKey, now);

    public RateLimitStatus Peek(Local db, string actorKey, Timestamp? now = null) =>
        Peek(k => db.RateLimitBucket.Key.Find(k), actorKey, now);

    /// <summary>From a view. Views have no clock, so an expired window still shows until swept.</summary>
    public RateLimitStatus Peek(ViewContext ctx, string actorKey) =>
        Peek(k => ctx.Db.RateLimitBucket.Key.Find(k), actorKey, null);

    /// <summary>
    /// Read the actor's bucket without spending. Pass <c>now</c> to report an expired window as
    /// fresh; without it, compare ResetAt against your own clock.
    /// </summary>
    RateLimitStatus Peek(Func<string, Module.RateLimitBucket?> find, string actorKey, Timestamp? now)
    {
        Check();
        var row = find(BucketKey(actorKey));
        if (row is not { } r || (now is { } n && r.ExpiresAt.MicrosecondsSinceUnixEpoch <= n.MicrosecondsSinceUnixEpoch))
        {
            return new RateLimitStatus(Limit, WindowSeconds, 0, Limit, null);
        }
        return new RateLimitStatus(Limit, WindowSeconds, r.Count, r.Count >= Limit ? 0 : Limit - r.Count, r.ExpiresAt);
    }

    static uint SecondsUntil(Timestamp now, Timestamp future)
    {
        var delta = future.MicrosecondsSinceUnixEpoch - now.MicrosecondsSinceUnixEpoch;
        if (delta <= 0) return 0;
        return (uint)Math.Min(uint.MaxValue, (delta + 999_999) / 1_000_000);
    }
}

public static partial class Module
{
    public const uint RateLimitDefaultSweepBatch = 500;
    public const uint RateLimitMaxSweepBatch = 10_000;
    const long RateLimitSweepIntervalMicros = 30_000_000;

    /// <summary>The rate_limit.* codes.</summary>
    public static class RateLimitErrors
    {
        public const string NotAuthorized = "rate_limit.not_authorized";
        public const string CannotRemoveLastAdmin = "rate_limit.cannot_remove_last_admin";
        public const string InvalidActorKey = "rate_limit.invalid_actor_key";
        public const string InvalidCost = "rate_limit.invalid_cost";
        public const string InvalidSweepBatch = "rate_limit.invalid_sweep_batch";
        public const string InvalidMaxRows = "rate_limit.invalid_max_rows";
        // Policy errors (TS throws these while the module loads).
        public const string InvalidScope = "rate_limit.invalid_scope";
        public const string DuplicateScope = "rate_limit.duplicate_scope";
        public const string InvalidLimit = "rate_limit.invalid_limit";
        public const string InvalidWindow = "rate_limit.invalid_window";
    }

    [SpacetimeDB.Table(Accessor = "RateLimitBucket")]
    public partial struct RateLimitBucket
    {
        [SpacetimeDB.PrimaryKey]
        public string Key;
        [SpacetimeDB.Index.BTree]
        public string Scope;
        [SpacetimeDB.Index.BTree]
        public Timestamp WindowStart;
        [SpacetimeDB.Index.BTree]
        public Timestamp ExpiresAt;
        public uint Count;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "RateLimitAdminIdentity")]
    public partial struct RateLimitAdminIdentity
    {
        [SpacetimeDB.PrimaryKey]
        public Identity Identity;
        public Timestamp AddedAt;
    }

    [SpacetimeDB.Table(Accessor = "RateLimitConfig", Public = true)]
    public partial struct RateLimitConfig
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public uint SweepBatch;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "RateLimitSweepTick", Scheduled = nameof(RateLimitSweep), ScheduledAt = nameof(RateLimitSweepTick.ScheduledAt))]
    public partial struct RateLimitSweepTick
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
    }

    static readonly Timestamp RateLimitTimestampMin = new(long.MinValue);

    /// <summary>
    /// Call from your Init reducer: the publisher becomes the first admin, and the
    /// expired-bucket sweep is scheduled every 30 s. Later calls do nothing.
    /// </summary>
    public static void RateLimitInstall(ReducerContext ctx)
    {
        if (ctx.Db.RateLimitConfig.Singleton.Find(true) is not null) return;
        ctx.Db.RateLimitAdminIdentity.Insert(new RateLimitAdminIdentity { Identity = ctx.Sender, AddedAt = ctx.Timestamp });
        ctx.Db.RateLimitConfig.Insert(new RateLimitConfig { Singleton = true, SweepBatch = RateLimitDefaultSweepBatch, UpdatedAt = ctx.Timestamp });
        ctx.Db.RateLimitSweepTick.Insert(new RateLimitSweepTick
        {
            ScheduledId = 0,
            ScheduledAt = new ScheduleAt.Interval(new TimeDuration(RateLimitSweepIntervalMicros)),
        });
    }

    public static bool RateLimitIsAdmin(Local db, Identity identity) => db.RateLimitAdminIdentity.Identity.Find(identity) is not null;

    /// <summary>Throws rate_limit.not_authorized unless the caller is a rate-limit admin.</summary>
    public static void RateLimitRequireAdmin(ReducerContext ctx) => RateLimitRequireAdmin(ctx.Db, ctx.Sender);

    public static void RateLimitRequireAdmin(Local db, Identity sender)
    {
        if (!RateLimitIsAdmin(db, sender)) throw new Exception(RateLimitErrors.NotAuthorized);
    }

    static uint RateLimitMaxRowsArg(uint? value) => value switch
    {
        null => RateLimitDefaultSweepBatch,
        >= 1 and <= RateLimitMaxSweepBatch => value.Value,
        _ => throw new Exception(RateLimitErrors.InvalidMaxRows),
    };

    /// <summary>Delete up to maxRows expired buckets. Returns how many were deleted.</summary>
    public static uint RateLimitSweepExpired(Local db, Timestamp now, uint maxRows)
    {
        if (maxRows < 1 || maxRows > RateLimitMaxSweepBatch) throw new Exception(RateLimitErrors.InvalidSweepBatch);
        var expired = db.RateLimitBucket.ExpiresAt.Filter(new Bound<Timestamp>(RateLimitTimestampMin, now))
            .Take((int)maxRows)
            .Select(b => b.Key)
            .ToList();
        foreach (var key in expired) db.RateLimitBucket.Key.Delete(key);
        return (uint)expired.Count;
    }

    /// <summary>Scheduled every 30 s by RateLimitInstall.</summary>
    [SpacetimeDB.Reducer]
    public static void RateLimitSweep(ReducerContext ctx, RateLimitSweepTick tick)
    {
        if (ctx.Sender != ctx.DatabaseIdentity) throw new Exception(RateLimitErrors.NotAuthorized);
        var batch = ctx.Db.RateLimitConfig.Singleton.Find(true)?.SweepBatch ?? RateLimitDefaultSweepBatch;
        RateLimitSweepExpired(ctx.Db, ctx.Timestamp, batch);
    }

    /// <summary>Sweep now (admins). Returns how many buckets were deleted. TS: run_sweep.</summary>
    [SpacetimeDB.Procedure]
    public static Result<uint, string> RateLimitRunSweep(ProcedureContext ctx, uint? maxRows)
    {
        try
        {
            var max = RateLimitMaxRowsArg(maxRows);
            return Result<uint, string>.Ok(ctx.WithTx(tx =>
            {
                RateLimitRequireAdmin(tx.Db, tx.Sender);
                return RateLimitSweepExpired(tx.Db, tx.Timestamp, max);
            }));
        }
        catch (Exception e)
        {
            return Result<uint, string>.Err(e.Message);
        }
    }

    [SpacetimeDB.Reducer]
    public static void AddRateLimitAdmin(ReducerContext ctx, Identity identity)
    {
        RateLimitRequireAdmin(ctx);
        if (ctx.Db.RateLimitAdminIdentity.Identity.Find(identity) is null)
        {
            ctx.Db.RateLimitAdminIdentity.Insert(new RateLimitAdminIdentity { Identity = identity, AddedAt = ctx.Timestamp });
        }
    }

    [SpacetimeDB.Reducer]
    public static void RemoveRateLimitAdmin(ReducerContext ctx, Identity identity)
    {
        RateLimitRequireAdmin(ctx);
        if (ctx.Db.RateLimitAdminIdentity.Identity.Find(identity) is null) return;
        if (ctx.Db.RateLimitAdminIdentity.Count <= 1) throw new Exception(RateLimitErrors.CannotRemoveLastAdmin);
        ctx.Db.RateLimitAdminIdentity.Identity.Delete(identity);
    }

    /// <summary>Set how many expired buckets each scheduled sweep deletes (admins). TS: update_config.</summary>
    [SpacetimeDB.Reducer]
    public static void RateLimitUpdateConfig(ReducerContext ctx, uint sweepBatch)
    {
        RateLimitRequireAdmin(ctx);
        var cfg = ctx.Db.RateLimitConfig.Singleton.Find(true) ?? throw new Exception("rate_limit.config_missing");
        if (sweepBatch < 1 || sweepBatch > RateLimitMaxSweepBatch) throw new Exception(RateLimitErrors.InvalidSweepBatch);
        ctx.Db.RateLimitConfig.Singleton.Update(cfg with { SweepBatch = sweepBatch, UpdatedAt = ctx.Timestamp });
    }

    /// <summary>Delete up to maxRows buckets, expired or not (admins). TS: reset_buckets.</summary>
    [SpacetimeDB.Reducer]
    public static void RateLimitResetBuckets(ReducerContext ctx, uint? maxRows)
    {
        RateLimitRequireAdmin(ctx);
        var max = RateLimitMaxRowsArg(maxRows);
        var keys = ctx.Db.RateLimitBucket.Iter().Take((int)max).Select(b => b.Key).ToList();
        foreach (var key in keys) ctx.Db.RateLimitBucket.Key.Delete(key);
    }

    /// <summary>Up to 1,000 buckets, for admins (others get nothing).</summary>
    [SpacetimeDB.View(Accessor = "AdminRateLimitBuckets", Public = true)]
    public static List<RateLimitBucket> AdminRateLimitBuckets(ViewContext ctx)
    {
        if (ctx.Db.RateLimitAdminIdentity.Identity.Find(ctx.Sender) is null) return [];
        // Views can't scan a table in 2.11, only read indexes: walk the whole ExpiresAt index.
        return ctx.Db.RateLimitBucket.ExpiresAt.Filter(new Bound<Timestamp>(RateLimitTimestampMin, new Timestamp(long.MaxValue)))
            .Take(1000)
            .ToList();
    }
}
