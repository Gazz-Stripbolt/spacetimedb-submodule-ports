// Presence.cs: who is here, with leases that expire, for C# modules.
//
// Unofficial C# port of @spacetimedb/presence (SpacetimeDB PR #5908, commit
// f6ed7dcd1c8f853c97fc7e9f126606fca7adadac, package spacetime-presence-ts). Licensed
// Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
// is by Tinker (Gazz-Stripbolt).
//
// Same tables (presence_entry, presence_config, presence_admin_identity, presence_sweep_tick),
// keys, limits, error codes and views (presence_online, presence_entries_admin) as the TS package.
//
//     [Reducer(ReducerKind.Init)]
//     public static void Init(ReducerContext ctx) => PresenceInstall(ctx);
//
//     // Your own scopes, from your own reducers:
//     PresenceUpsert(ctx, new PresenceUpsertOpts($"room:{room}", ctx.Sender.ToString()) { Status = "typing" });
//
// Changes from the TypeScript original:
// - The generic names are prefixed: heartbeat -> presence_heartbeat, run_sweep ->
//   presence_run_sweep, update_config -> presence_update_config.
// - Procedures return Result<T, string>; reducers and helpers throw Exceptions carrying the
//   same presence.* codes.
// - presence_sweep rejects callers other than the scheduler.
// - presence_entries_admin walks the ExpiresAt index: C# views can't scan tables.

#pragma warning disable STDB_UNSTABLE
#nullable enable

using SpacetimeDB;

/// <summary>What presence_heartbeat returns.</summary>
[SpacetimeDB.Type]
public partial struct PresenceHeartbeatResult
{
    public string Scope;
    public string Subject;
    public string Status;
    public Timestamp ExpiresAt;
}

/// <summary>Options for PresenceUpsert. Status defaults to "online", TtlSeconds to the configured default.</summary>
public record PresenceUpsertOpts(string Scope, string Subject)
{
    public string? Status { get; init; }
    public string? Activity { get; init; }
    public string? PayloadJson { get; init; }
    public uint? TtlSeconds { get; init; }
}

public static partial class Module
{
    public const uint PresenceDefaultTtlSeconds = 30;
    public const uint PresenceDefaultSweepBatch = 500;
    public const uint PresenceMaxSweepBatch = 10_000;
    public const string PresenceDefaultStatus = "online";
    /// <summary>The scope the client-callable operations write. Hosts choose their own scopes.</summary>
    public const string PresenceGlobalScope = "presence.global";
    const int PresenceMaxScopeLength = 128;
    const int PresenceMaxSubjectLength = 256;
    const int PresenceMaxStatusLength = 64;
    const int PresenceMaxActivityLength = 256;
    const int PresenceMaxPayloadLength = 4096;
    const uint PresenceMaxTtlSeconds = 3600;
    const int PresenceMaxViewRows = 1000;
    const long PresenceSweepIntervalMicros = 10_000_000;

    /// <summary>The presence.* codes.</summary>
    public static class PresenceErrors
    {
        public const string NotAuthorized = "presence.not_authorized";
        public const string CannotRemoveLastAdmin = "presence.cannot_remove_last_admin";
        public const string ConfigMissing = "presence.config_missing";
        public const string InvalidScope = "presence.invalid_scope";
        public const string InvalidSubject = "presence.invalid_subject";
        public const string InvalidStatus = "presence.invalid_status";
        public const string InvalidActivity = "presence.invalid_activity";
        public const string InvalidPayload = "presence.invalid_payload";
        public const string InvalidTtlSeconds = "presence.invalid_ttl_seconds";
        public const string InvalidSweepBatch = "presence.invalid_sweep_batch";
    }

    [SpacetimeDB.Table(Accessor = "PresenceEntry")]
    public partial struct PresenceEntry
    {
        [SpacetimeDB.PrimaryKey]
        public string Key;
        [SpacetimeDB.Index.BTree]
        public string Scope;
        [SpacetimeDB.Index.BTree]
        public string Subject;
        [SpacetimeDB.Index.BTree]
        public string Status;
        public string? Activity;
        public string? PayloadJson;
        [SpacetimeDB.Index.BTree]
        public Timestamp JoinedAt;
        [SpacetimeDB.Index.BTree]
        public Timestamp LastSeenAt;
        [SpacetimeDB.Index.BTree]
        public Timestamp ExpiresAt;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "PresenceConfig")]
    public partial struct PresenceConfig
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public uint DefaultTtlSeconds;
        public uint SweepBatch;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "PresenceAdminIdentity")]
    public partial struct PresenceAdminIdentity
    {
        [SpacetimeDB.PrimaryKey]
        public Identity Identity;
        public Timestamp AddedAt;
    }

    [SpacetimeDB.Table(Accessor = "PresenceSweepTick", Scheduled = nameof(PresenceSweep), ScheduledAt = nameof(PresenceSweepTick.ScheduledAt))]
    public partial struct PresenceSweepTick
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
    }

    static readonly Timestamp PresenceTimestampMin = new(long.MinValue);

    static string PresenceSanitize(string code, string value, int maxLength)
    {
        var o = value.Trim();
        if (o.Length == 0 || o.Length > maxLength) throw new Exception(code);
        return o;
    }

    static void PresenceAssertTtl(uint ttlSeconds)
    {
        if (ttlSeconds == 0 || ttlSeconds > PresenceMaxTtlSeconds) throw new Exception(PresenceErrors.InvalidTtlSeconds);
    }

    static void PresenceAssertSweepBatch(uint value)
    {
        if (value == 0 || value > PresenceMaxSweepBatch) throw new Exception(PresenceErrors.InvalidSweepBatch);
    }

    static Timestamp PresencePlusSeconds(Timestamp ts, uint seconds) =>
        new(ts.MicrosecondsSinceUnixEpoch + (long)seconds * 1_000_000);

    static uint PresenceDefaultTtl(Local db) =>
        db.PresenceConfig.Singleton.Find(true)?.DefaultTtlSeconds ?? PresenceDefaultTtlSeconds;

    /// <summary>`opts.activity?.trim() || undefined`</summary>
    static string? PresenceTrimmedOrNull(string? value) => value?.Trim() is { Length: > 0 } s ? s : null;

    /// <summary>The stored key: both parts trimmed and length-prefixed (UTF-16 lengths, as in TS) so delimiters can't collide.</summary>
    public static string PresenceBuildKey(string scope, string subject)
    {
        var s = PresenceSanitize(PresenceErrors.InvalidScope, scope, PresenceMaxScopeLength);
        var u = PresenceSanitize(PresenceErrors.InvalidSubject, subject, PresenceMaxSubjectLength);
        return $"{s.Length}:{s}{u.Length}:{u}";
    }

    /// <summary>Inserts the config row once. Later calls keep the stored config.</summary>
    public static void PresenceInstallConfig(Local db, Timestamp now, uint defaultTtlSeconds = PresenceDefaultTtlSeconds, uint sweepBatch = PresenceDefaultSweepBatch)
    {
        PresenceAssertTtl(defaultTtlSeconds);
        PresenceAssertSweepBatch(sweepBatch);
        if (db.PresenceConfig.Singleton.Find(true) is not null) return;
        db.PresenceConfig.Insert(new PresenceConfig { Singleton = true, DefaultTtlSeconds = defaultTtlSeconds, SweepBatch = sweepBatch, UpdatedAt = now });
    }

    public static void PresenceSetConfig(Local db, Timestamp now, uint defaultTtlSeconds, uint sweepBatch)
    {
        PresenceAssertTtl(defaultTtlSeconds);
        PresenceAssertSweepBatch(sweepBatch);
        var existing = db.PresenceConfig.Singleton.Find(true) ?? throw new Exception(PresenceErrors.ConfigMissing);
        db.PresenceConfig.Singleton.Update(existing with { DefaultTtlSeconds = defaultTtlSeconds, SweepBatch = sweepBatch, UpdatedAt = now });
    }

    public static PresenceEntry PresenceUpsert(ReducerContext ctx, PresenceUpsertOpts opts) => PresenceUpsert(ctx.Db, ctx.Timestamp, opts);

    /// <summary>Insert or refresh a presence entry, in the caller's transaction (use tx.Db and tx.Timestamp from a procedure).</summary>
    public static PresenceEntry PresenceUpsert(Local db, Timestamp now, PresenceUpsertOpts opts)
    {
        var scope = PresenceSanitize(PresenceErrors.InvalidScope, opts.Scope, PresenceMaxScopeLength);
        var subject = PresenceSanitize(PresenceErrors.InvalidSubject, opts.Subject, PresenceMaxSubjectLength);
        var key = PresenceBuildKey(scope, subject);
        var ttl = opts.TtlSeconds ?? PresenceDefaultTtl(db);
        PresenceAssertTtl(ttl);
        var status = PresenceSanitize(PresenceErrors.InvalidStatus, opts.Status ?? PresenceDefaultStatus, PresenceMaxStatusLength);
        var activity = PresenceTrimmedOrNull(opts.Activity);
        if ((activity?.Length ?? 0) > PresenceMaxActivityLength) throw new Exception(PresenceErrors.InvalidActivity);
        var payload = PresenceTrimmedOrNull(opts.PayloadJson);
        if ((payload?.Length ?? 0) > PresenceMaxPayloadLength) throw new Exception(PresenceErrors.InvalidPayload);

        var expiresAt = PresencePlusSeconds(now, ttl);
        if (db.PresenceEntry.Key.Find(key) is not { } existing)
        {
            return db.PresenceEntry.Insert(new PresenceEntry
            {
                Key = key,
                Scope = scope,
                Subject = subject,
                Status = status,
                Activity = activity,
                PayloadJson = payload,
                JoinedAt = now,
                LastSeenAt = now,
                ExpiresAt = expiresAt,
                UpdatedAt = now,
            });
        }
        return db.PresenceEntry.Key.Update(existing with
        {
            Status = status,
            Activity = activity,
            PayloadJson = payload,
            LastSeenAt = now,
            ExpiresAt = expiresAt,
            UpdatedAt = now,
        });
    }

    public static PresenceEntry PresenceTouch(ReducerContext ctx, string scope, string subject, uint? ttlSeconds = null) =>
        PresenceTouch(ctx.Db, ctx.Timestamp, scope, subject, ttlSeconds);

    /// <summary>Extends a lease and keeps its status, activity and payload. Creates the entry when there is none.</summary>
    public static PresenceEntry PresenceTouch(Local db, Timestamp now, string scope, string subject, uint? ttlSeconds = null)
    {
        var key = PresenceBuildKey(scope, subject);
        if (db.PresenceEntry.Key.Find(key) is not { } existing)
        {
            return PresenceUpsert(db, now, new PresenceUpsertOpts(scope, subject) { TtlSeconds = ttlSeconds });
        }
        var ttl = ttlSeconds ?? PresenceDefaultTtl(db);
        PresenceAssertTtl(ttl);
        return db.PresenceEntry.Key.Update(existing with { LastSeenAt = now, ExpiresAt = PresencePlusSeconds(now, ttl), UpdatedAt = now });
    }

    public static bool PresenceRemove(ReducerContext ctx, string scope, string subject) => PresenceRemove(ctx.Db, scope, subject);

    /// <summary>Delete an entry. Returns whether one existed.</summary>
    public static bool PresenceRemove(Local db, string scope, string subject) =>
        db.PresenceEntry.Key.Delete(PresenceBuildKey(scope, subject));

    /// <summary>Deletes up to the configured sweep batch of rows that have expired. Returns the count.</summary>
    public static uint PresenceRunSweepNow(Local db, Timestamp now)
    {
        var batch = db.PresenceConfig.Singleton.Find(true)?.SweepBatch ?? PresenceDefaultSweepBatch;
        var expired = db.PresenceEntry.ExpiresAt.Filter(new Bound<Timestamp>(PresenceTimestampMin, now))
            .Take((int)batch)
            .Select(r => r.Key)
            .ToList();
        foreach (var key in expired) db.PresenceEntry.Key.Delete(key);
        return (uint)expired.Count;
    }

    // -----------------------------------------------------------------------
    // Install, admin, client operations
    // -----------------------------------------------------------------------

    /// <summary>Call from your Init reducer: the caller becomes a presence admin, config is seeded, and expiry sweeps run every 10 s.</summary>
    public static void PresenceInstall(ReducerContext ctx)
    {
        if (ctx.Db.PresenceAdminIdentity.Identity.Find(ctx.Sender) is null)
        {
            ctx.Db.PresenceAdminIdentity.Insert(new PresenceAdminIdentity { Identity = ctx.Sender, AddedAt = ctx.Timestamp });
        }
        PresenceInstallConfig(ctx.Db, ctx.Timestamp);
        if (ctx.Db.PresenceSweepTick.Count == 0)
        {
            ctx.Db.PresenceSweepTick.Insert(new PresenceSweepTick
            {
                ScheduledId = 0,
                ScheduledAt = new ScheduleAt.Interval(new TimeDuration(PresenceSweepIntervalMicros)),
            });
        }
    }

    public static bool PresenceIsAdmin(Local db, Identity identity) => db.PresenceAdminIdentity.Identity.Find(identity) is not null;

    /// <summary>Throws presence.not_authorized unless the caller is a presence admin.</summary>
    public static void PresenceRequireAdmin(Local db, Identity sender)
    {
        if (!PresenceIsAdmin(db, sender)) throw new Exception(PresenceErrors.NotAuthorized);
    }

    /// <summary>The hex form TS's identity.toHexString() produces (lowercase, no 0x).</summary>
    static string PresenceIdentityHex(Identity identity)
    {
        var s = identity.ToString();
        if (s.StartsWith("0x", StringComparison.OrdinalIgnoreCase)) s = s[2..];
        return s.ToLowerInvariant();
    }

    /// <summary>Records the caller's global presence. The subject is the caller's identity (hex). TS: heartbeat.</summary>
    [SpacetimeDB.Procedure]
    public static Result<PresenceHeartbeatResult, string> PresenceHeartbeat(ProcedureContext ctx, string? status, string? activity, string? payloadJson, uint? ttlSeconds)
    {
        try
        {
            var subject = PresenceIdentityHex(ctx.Sender);
            var row = ctx.WithTx(tx => PresenceUpsert(tx.Db, tx.Timestamp, new PresenceUpsertOpts(PresenceGlobalScope, subject)
            {
                Status = status,
                Activity = activity,
                PayloadJson = payloadJson,
                TtlSeconds = ttlSeconds,
            }));
            return Result<PresenceHeartbeatResult, string>.Ok(new PresenceHeartbeatResult
            {
                Scope = row.Scope,
                Subject = row.Subject,
                Status = row.Status,
                ExpiresAt = row.ExpiresAt,
            });
        }
        catch (Exception e)
        {
            return Result<PresenceHeartbeatResult, string>.Err(e.Message);
        }
    }

    [SpacetimeDB.Reducer]
    public static void ClearPresence(ReducerContext ctx) => PresenceRemove(ctx.Db, PresenceGlobalScope, PresenceIdentityHex(ctx.Sender));

    /// <summary>Sweep now (admins). Returns how many entries were deleted. TS: run_sweep.</summary>
    [SpacetimeDB.Procedure]
    public static Result<uint, string> PresenceRunSweep(ProcedureContext ctx)
    {
        try
        {
            return Result<uint, string>.Ok(ctx.WithTx(tx =>
            {
                PresenceRequireAdmin(tx.Db, tx.Sender);
                return PresenceRunSweepNow(tx.Db, tx.Timestamp);
            }));
        }
        catch (Exception e)
        {
            return Result<uint, string>.Err(e.Message);
        }
    }

    [SpacetimeDB.Reducer]
    public static void AddPresenceAdmin(ReducerContext ctx, Identity identity)
    {
        PresenceRequireAdmin(ctx.Db, ctx.Sender);
        if (ctx.Db.PresenceAdminIdentity.Identity.Find(identity) is null)
        {
            ctx.Db.PresenceAdminIdentity.Insert(new PresenceAdminIdentity { Identity = identity, AddedAt = ctx.Timestamp });
        }
    }

    [SpacetimeDB.Reducer]
    public static void RemovePresenceAdmin(ReducerContext ctx, Identity identity)
    {
        PresenceRequireAdmin(ctx.Db, ctx.Sender);
        if (ctx.Db.PresenceAdminIdentity.Identity.Find(identity) is null) return;
        if (ctx.Db.PresenceAdminIdentity.Count <= 1) throw new Exception(PresenceErrors.CannotRemoveLastAdmin);
        ctx.Db.PresenceAdminIdentity.Identity.Delete(identity);
    }

    /// <summary>Admins. TS: update_config.</summary>
    [SpacetimeDB.Reducer]
    public static void PresenceUpdateConfig(ReducerContext ctx, uint defaultTtlSeconds, uint sweepBatch)
    {
        PresenceRequireAdmin(ctx.Db, ctx.Sender);
        PresenceSetConfig(ctx.Db, ctx.Timestamp, defaultTtlSeconds, sweepBatch);
    }

    /// <summary>Up to 1,000 global presence entries, including their activity and payload.</summary>
    [SpacetimeDB.View(Accessor = "PresenceOnline", Public = true)]
    public static List<PresenceEntry> PresenceOnline(ViewContext ctx) =>
        ctx.Db.PresenceEntry.Scope.Filter(PresenceGlobalScope).Take(PresenceMaxViewRows).ToList();

    /// <summary>Up to 1,000 entries in every scope, for admins (others get nothing).</summary>
    [SpacetimeDB.View(Accessor = "PresenceEntriesAdmin", Public = true)]
    public static List<PresenceEntry> PresenceEntriesAdmin(ViewContext ctx)
    {
        if (ctx.Db.PresenceAdminIdentity.Identity.Find(ctx.Sender) is null) return [];
        // Views can't scan a table in 2.11, only read indexes: walk the whole ExpiresAt index.
        return ctx.Db.PresenceEntry.ExpiresAt.Filter(new Bound<Timestamp>(PresenceTimestampMin, new Timestamp(long.MaxValue)))
            .Take(PresenceMaxViewRows)
            .ToList();
    }

    /// <summary>Scheduled every 10 s by PresenceInstall.</summary>
    [SpacetimeDB.Reducer]
    public static void PresenceSweep(ReducerContext ctx, PresenceSweepTick tick)
    {
        if (ctx.Sender != ctx.DatabaseIdentity) throw new Exception(PresenceErrors.NotAuthorized);
        PresenceRunSweepNow(ctx.Db, ctx.Timestamp);
    }
}
