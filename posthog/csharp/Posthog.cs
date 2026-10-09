// Posthog.cs: PostHog analytics (a durable event outbox) and feature flags, for C# modules.
//
// Unofficial C# port of @spacetimedb/posthog (SpacetimeDB PR #5907, commit
// b1063b1876f4d7b8c723adb54b3d0f4d248b9eb0, package spacetime-posthog-ts). Licensed Apache-2.0,
// like the original. Original design and behaviour by Clockwork Labs; this port is by Tinker
// (Gazz-Stripbolt).
//
// Queue events in the same transaction as your own changes; a scheduled procedure sends them to
// PostHog's /batch endpoint every 5 seconds with retries and backoff, and admins can capture
// directly, evaluate feature flags, requeue failures and inspect delivery. Same tables, views,
// error codes, limits and retry rules as the TS package.
//
//     public static partial class Module
//     {
//         [Reducer(ReducerKind.Init)]
//         public static void Init(ReducerContext ctx) => PosthogInstall(ctx);
//
//         [Reducer]
//         public static void CompleteOrder(ReducerContext ctx, string orderId)
//         {
//             // … your order mutation …
//             PosthogEnqueueEventInTx(ctx, new PosthogEnqueueEventArgs
//             {
//                 DistinctId = ctx.Sender.ToString().ToLowerInvariant(), Event = "order_completed",
//                 PropertiesJson = $"{{\"orderId\":\"{orderId}\"}}", IdempotencyKey = $"order_completed:{orderId}",
//             });
//         }
//     }
//
// Changes from the TypeScript original:
// - Names: generic procedures and reducers get a posthog_ prefix (enqueue_event →
//   posthog_enqueue_event, capture_now, flush_outbox, requeue_failed_events, clear_analytics,
//   get_feature_flag, scheduled_flush, add/remove_admin_identity likewise).
// - Procedures return Result<T, string> instead of throwing; the scheduled flush keeps the TS
//   unit return and throws for callers other than the scheduler.
// - JSON sent to PostHog is serialized like JS JSON.stringify (integer-like keys first).

#pragma warning disable STDB_UNSTABLE
#nullable enable

using System.Globalization;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using SpacetimeDB;
using HttpMethod = SpacetimeDB.HttpMethod;

[SpacetimeDB.Type]
public enum PostHogOutboxStatus
{
    Queued,
    Processing,
    Delivered,
    Failed,
    Rejected,
}

[SpacetimeDB.Type]
public enum PostHogDeliverySource
{
    Direct,
    Flush,
    FeatureFlag,
}

[SpacetimeDB.Type]
public partial struct PostHogDeliveryLogRow
{
    public ulong DeliveryId;
    public PostHogDeliverySource Source;
    public string? OutboxId;
    public string DistinctId;
    public string Event;
    public bool Ok;
    public ushort StatusCode;
    public string ResponseBody;
    public string? ErrorMessage;
    public Timestamp AttemptedAt;
}

[SpacetimeDB.Type]
public partial struct PostHogConfigStatus
{
    public bool IsConfigured;
    public string? Host;
    public uint ProjectApiKeyLength;
}

[SpacetimeDB.Type]
public partial struct PostHogFlushResult
{
    public uint Attempted;
    public uint Delivered;
    public uint Failed;
}

[SpacetimeDB.Type]
public partial struct PostHogCaptureResult
{
    public bool Ok;
    public ushort StatusCode;
    public string? Error;
}

[SpacetimeDB.Type]
public partial struct PostHogFeatureFlagResult
{
    public bool Ok;
    public ushort StatusCode;
    public bool? Enabled;
    public string? Variant;
    public string? Error;
}

/// <summary>Arguments for PosthogEnqueueEventInTx.</summary>
public sealed class PosthogEnqueueEventArgs
{
    public string DistinctId { get; init; } = "";
    public string Event { get; init; } = "";
    public string? PropertiesJson { get; init; }
    public string? IdempotencyKey { get; init; }
}

/// <summary>What PosthogEnqueueEventInTx did: Error is the rejection code for invalid input.</summary>
public readonly record struct PosthogEnqueueEventResult(string OutboxId, bool Inserted, string? Error);

/// <summary>A PostHog response. Network failures are status 0 with the error as the body.</summary>
public readonly record struct PosthogHttpResult(bool Ok, ushort StatusCode, string ResponseBody);

public static partial class Module
{
    public static class PosthogErrors
    {
        public const string NotAuthorized = "posthog.not_authorized";
        public const string CannotRemoveLastAdmin = "posthog.cannot_remove_last_admin";
        public const string ConfigMissing = "posthog.config_missing";
        public const string InvalidHost = "posthog.invalid_host";
        public const string InvalidProjectApiKey = "posthog.invalid_project_api_key";
        public const string InvalidDistinctId = "posthog.invalid_distinct_id";
        public const string InvalidEvent = "posthog.invalid_event";
        public const string InvalidPropertiesJson = "posthog.invalid_properties_json";
        public const string PropertiesTooLarge = "posthog.properties_too_large";
        public const string IdempotencyKeyTooLong = "posthog.idempotency_key_too_long";
        public const string InvalidFlagKey = "posthog.invalid_flag_key";
        public const string InvalidPersonPropertiesJson = "posthog.invalid_person_properties_json";
        public const string PersonPropertiesTooLarge = "posthog.person_properties_too_large";
        public const string InvalidGroupsJson = "posthog.invalid_groups_json";
        public const string GroupsTooLarge = "posthog.groups_too_large";
        public const string InvalidFlushLimit = "posthog.invalid_flush_limit";
        public const string InvalidClearBatch = "posthog.invalid_clear_batch";
        public const string InvalidRequeueLimit = "posthog.invalid_requeue_limit";
    }

    public const long PosthogFlushIntervalMicros = 5 * 1_000_000;
    public const uint PosthogMaxDeliveryAttempts = 5;
    const long PosthogInitialRetryDelayMicros = 1_000_000;
    const long PosthogMaxRetryDelayMicros = 5L * 60 * 1_000_000;
    const int PosthogMaxLogBodyLength = 2048;
    public const uint PosthogMaxFlushLimit = 100;
    const int PosthogMaxFlushBatchesPerTick = 10;
    const long PosthogClaimTtlMicros = 5L * 60 * 1_000_000;
    const int PosthogMaxExpiredClaimsPerFlush = 1_000;
    const int PosthogMaxDistinctIdLength = 256;
    const int PosthogMaxEventNameLength = 200;
    const int PosthogMaxPropertiesJsonLength = 64 * 1024;
    const int PosthogMaxIdempotencyKeyLength = 256;
    const long PosthogHistoryRetentionMicros = 30L * 24 * 60 * 60 * 1_000_000;
    const int PosthogMaxRetentionRowsPerTick = 1_000;
    const uint PosthogMaxAdminBatch = 10_000;

    // -----------------------------------------------------------------------
    // Tables
    // -----------------------------------------------------------------------

    [SpacetimeDB.Table(Accessor = "PosthogConfig")]
    public partial struct PostHogConfig
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public string Host;
        public string ProjectApiKey;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "PosthogAdminIdentity")]
    public partial struct PostHogAdminIdentity
    {
        [SpacetimeDB.PrimaryKey]
        public Identity Identity;
        public Timestamp AddedAt;
    }

    [SpacetimeDB.Table(Accessor = "PosthogOutbox")]
    [SpacetimeDB.Index.BTree(Accessor = "ByStatusCreatedAt", Columns = new[] { nameof(Status), nameof(CreatedAt) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByStatusNextAttemptAt", Columns = new[] { nameof(Status), nameof(NextAttemptAt) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByStatusClaimExpiresAt", Columns = new[] { nameof(Status), nameof(ClaimExpiresAt) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByStatusUpdatedAt", Columns = new[] { nameof(Status), nameof(UpdatedAt) })]
    public partial struct PostHogOutbox
    {
        [SpacetimeDB.PrimaryKey]
        public string OutboxId;
        public string Uuid;
        public string? IdempotencyKey;
        public string DistinctId;
        public string Event;
        public string? PropertiesJson;
        [SpacetimeDB.Index.BTree]
        public PostHogOutboxStatus Status;
        public uint Attempts;
        public string? ClaimId;
        public Timestamp ClaimExpiresAt;
        public Timestamp NextAttemptAt;
        public ushort? LastStatusCode;
        public string? LastError;
        [SpacetimeDB.Index.BTree]
        public Timestamp CreatedAt;
        public Timestamp UpdatedAt;
        public Timestamp? DeliveredAt;
    }

    /// <summary>Runs posthog_scheduled_flush every 5 seconds (created by PosthogInstall).</summary>
    [SpacetimeDB.Table(Accessor = "PosthogFlushTick", Scheduled = nameof(PosthogScheduledFlush), ScheduledAt = nameof(PostHogFlushTick.ScheduledAt))]
    public partial struct PostHogFlushTick
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
    }

    /// <summary>Outbox row counts: Pending is Queued + Processing, Failed is Failed + Rejected.</summary>
    [SpacetimeDB.Table(Accessor = "PosthogDeliveryStats")]
    public partial struct PostHogDeliveryStats
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public ulong Pending;
        public ulong Delivered;
        public ulong Failed;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "PosthogDeliveryLog")]
    public partial struct PostHogDeliveryLog
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong DeliveryId;
        public PostHogDeliverySource Source;
        public string? OutboxId;
        public string DistinctId;
        public string Event;
        [SpacetimeDB.Index.BTree]
        public bool Ok;
        public ushort StatusCode;
        public string ResponseBody;
        public string? ErrorMessage;
        [SpacetimeDB.Index.BTree]
        public Timestamp AttemptedAt;
        /// <summary>-AttemptedAt in micros, so the index reads newest first.</summary>
        [SpacetimeDB.Index.BTree]
        public long AttemptedAtOrder;
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    static string PosthogSlice(string s, int n)
    {
        if (s.Length <= n) return s;
        if (n > 0 && char.IsHighSurrogate(s[n - 1])) n--;
        return s[..n];
    }

    /// <summary>Keeps the first 2,048 characters plus "...".</summary>
    public static string PosthogTruncateForLog(string body) =>
        body.Length <= PosthogMaxLogBodyLength ? body : body[..PosthogMaxLogBodyLength] + "...";

    public static bool PosthogIsOkStatus(ushort status) => status is >= 200 and < 300;

    /// <summary>4xx responses other than 408 and 429 won't succeed on retry.</summary>
    public static bool PosthogIsPermanentFailure(ushort status) => status is >= 400 and < 500 && status != 408 && status != 429;

    /// <summary>1 s doubling per attempt, capped at 5 minutes.</summary>
    public static long PosthogRetryDelayMicros(uint attempt) =>
        Math.Min(PosthogInitialRetryDelayMicros << (int)Math.Clamp((long)attempt - 1, 0, 30), PosthogMaxRetryDelayMicros);

    /// <summary>https:// or http:// plus a host; trailing slashes dropped. Throws posthog.invalid_host.</summary>
    public static string PosthogNormalizeHost(string host)
    {
        var trimmed = host.Trim();
        var lower = trimmed.ToLowerInvariant();
        var rest = lower.StartsWith("https://", StringComparison.Ordinal) ? lower[8..]
            : lower.StartsWith("http://", StringComparison.Ordinal) ? lower[7..] : null;
        if (rest is null || rest.Length == 0 || rest[0] == '/') throw new Exception(PosthogErrors.InvalidHost);
        return trimmed.TrimEnd('/');
    }

    /// <summary>A JSON object (not an array or null), else null.</summary>
    public static JsonObject? PosthogParseJsonObject(string json)
    {
        try { return JsonNode.Parse(json) as JsonObject; } catch (Exception) { return null; }
    }

    static bool PosthogIsArrayIndex(string k) =>
        k.Length > 0 && k.All(c => c is >= '0' and <= '9') && (k == "0" || k[0] != '0')
        && ulong.TryParse(k, NumberStyles.None, CultureInfo.InvariantCulture, out var n) && n < 4_294_967_295;

    static string PosthogJsString(string s)
    {
        var sb = new StringBuilder(s.Length + 2).Append('"');
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
                    if (c < 0x20) sb.Append("\\u").Append(((int)c).ToString("x4", CultureInfo.InvariantCulture));
                    else sb.Append(c);
                    break;
            }
        }
        return sb.Append('"').ToString();
    }

    static string PosthogJsNumber(JsonValue v)
    {
        if (v.TryGetValue<long>(out var l)) return l.ToString(CultureInfo.InvariantCulture);
        var d = v.GetValue<double>();
        if (Math.Abs(d) < 1e21 && Math.Floor(d) == d) return d.ToString("F0", CultureInfo.InvariantCulture);
        return d.ToString("R", CultureInfo.InvariantCulture);
    }

    /// <summary>JSON.stringify as JS writes it: integer-like keys first, non-ASCII raw.</summary>
    public static string PosthogJsJson(JsonNode? v) => v switch
    {
        null => "null",
        JsonObject o => "{" + string.Join(",", o.Where(kv => PosthogIsArrayIndex(kv.Key)).OrderBy(kv => ulong.Parse(kv.Key, CultureInfo.InvariantCulture))
            .Concat(o.Where(kv => !PosthogIsArrayIndex(kv.Key)))
            .Select(kv => PosthogJsString(kv.Key) + ":" + PosthogJsJson(kv.Value))) + "}",
        JsonArray a => "[" + string.Join(",", a.Select(PosthogJsJson)) + "]",
        JsonValue x => x.GetValueKind() switch
        {
            JsonValueKind.String => PosthogJsString(x.GetValue<string>()),
            JsonValueKind.Number => PosthogJsNumber(x),
            JsonValueKind.True => "true",
            JsonValueKind.False => "false",
            _ => "null",
        },
        _ => "null",
    };

    /// <summary>The TS SDK's Timestamp.toISOString(): YYYY-MM-DDTHH:MM:SS.ffffffZ.</summary>
    public static string PosthogIsoString(Timestamp t) =>
        DateTime.UnixEpoch.AddTicks(t.MicrosecondsSinceUnixEpoch * 10).ToString("yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'", CultureInfo.InvariantCulture);

    static Timestamp PosthogPlus(Timestamp t, long micros) => new(t.MicrosecondsSinceUnixEpoch + micros);

    static readonly Timestamp PosthogEpoch = new(0);
    static readonly Timestamp PosthogMinTime = new(long.MinValue);

    static int PosthogStatsKey(PostHogOutboxStatus s) => s switch
    {
        PostHogOutboxStatus.Delivered => 1,
        PostHogOutboxStatus.Failed or PostHogOutboxStatus.Rejected => 2,
        _ => 0,
    };

    sealed class PosthogStatsDelta
    {
        readonly long[] d = new long[3];

        public void Change(PostHogOutboxStatus? from, PostHogOutboxStatus? to)
        {
            if (from is { } f) d[PosthogStatsKey(f)]--;
            if (to is { } t) d[PosthogStatsKey(t)]++;
        }

        public void Apply(Local db, Timestamp now)
        {
            if (d[0] == 0 && d[1] == 0 && d[2] == 0) return;
            var existing = db.PosthogDeliveryStats.Singleton.Find(true);
            var current = existing ?? new PostHogDeliveryStats { Singleton = true, UpdatedAt = now };
            ulong Adjust(ulong v, long delta) => (ulong)Math.Max(0, (long)v + delta);
            var row = new PostHogDeliveryStats
            {
                Singleton = true,
                Pending = Adjust(current.Pending, d[0]),
                Delivered = Adjust(current.Delivered, d[1]),
                Failed = Adjust(current.Failed, d[2]),
                UpdatedAt = now,
            };
            if (existing is not null) db.PosthogDeliveryStats.Singleton.Update(row);
            else db.PosthogDeliveryStats.Insert(row);
        }
    }

    // -----------------------------------------------------------------------
    // Admins and config
    // -----------------------------------------------------------------------

    /// <summary>Call from your Init reducer: seeds the publishing identity and starts the 5-second flush.</summary>
    public static void PosthogInstall(ReducerContext ctx)
    {
        if (ctx.Db.PosthogAdminIdentity.Identity.Find(ctx.Sender) is null)
        {
            ctx.Db.PosthogAdminIdentity.Insert(new PostHogAdminIdentity { Identity = ctx.Sender, AddedAt = ctx.Timestamp });
        }
        if (ctx.Db.PosthogFlushTick.Count == 0)
        {
            ctx.Db.PosthogFlushTick.Insert(new PostHogFlushTick
            {
                ScheduledId = 0,
                ScheduledAt = new ScheduleAt.Interval(new TimeDuration(PosthogFlushIntervalMicros)),
            });
        }
    }

    public static bool PosthogIsAdmin(Local db, Identity who) => db.PosthogAdminIdentity.Identity.Find(who) is not null;

    static void PosthogRequireAdmin(Local db, Identity who)
    {
        if (!PosthogIsAdmin(db, who)) throw new Exception(PosthogErrors.NotAuthorized);
    }

    static Result<T, string> PosthogTry<T>(Func<T> f)
    {
        try { return Result<T, string>.Ok(f()); }
        catch (Exception e) { return Result<T, string>.Err(e.Message); }
    }

    static void PosthogRequireProcedureAdmin(ProcedureContext ctx)
    {
        var sender = ctx.Sender;
        if (!ctx.WithTx(tx => PosthogIsAdmin(tx.Db, sender))) throw new Exception(PosthogErrors.NotAuthorized);
    }

    [SpacetimeDB.Procedure]
    public static Result<Unit, string> PosthogAddAdminIdentity(ProcedureContext ctx, Identity identity) => PosthogTry(() =>
    {
        PosthogRequireProcedureAdmin(ctx);
        var now = ctx.Timestamp;
        ctx.WithTx(tx =>
        {
            if (tx.Db.PosthogAdminIdentity.Identity.Find(identity) is null)
            {
                tx.Db.PosthogAdminIdentity.Insert(new PostHogAdminIdentity { Identity = identity, AddedAt = now });
            }
            return 0;
        });
        return default(Unit);
    });

    [SpacetimeDB.Procedure]
    public static Result<Unit, string> PosthogRemoveAdminIdentity(ProcedureContext ctx, Identity identity) => PosthogTry(() =>
    {
        PosthogRequireProcedureAdmin(ctx);
        var error = ctx.WithTx(tx =>
        {
            if (tx.Db.PosthogAdminIdentity.Identity.Find(identity) is null) return null;
            if (tx.Db.PosthogAdminIdentity.Count <= 1) return PosthogErrors.CannotRemoveLastAdmin;
            tx.Db.PosthogAdminIdentity.Identity.Delete(identity);
            return (string?)null;
        });
        if (error is not null) throw new Exception(error);
        return default(Unit);
    });

    /// <summary>Admins only. host must start with https:// or http://.</summary>
    [SpacetimeDB.Procedure]
    public static Result<Unit, string> SetPosthogConfig(ProcedureContext ctx, string host, string projectApiKey) => PosthogTry(() =>
    {
        var normalized = PosthogNormalizeHost(host);
        var key = projectApiKey.Trim();
        if (key.Length == 0) throw new Exception(PosthogErrors.InvalidProjectApiKey);
        PosthogRequireProcedureAdmin(ctx);
        var now = ctx.Timestamp;
        ctx.WithTx(tx =>
        {
            var row = new PostHogConfig { Singleton = true, Host = normalized, ProjectApiKey = key, UpdatedAt = now };
            if (tx.Db.PosthogConfig.Singleton.Find(true) is not null) tx.Db.PosthogConfig.Singleton.Update(row);
            else tx.Db.PosthogConfig.Insert(row);
            return 0;
        });
        return default(Unit);
    });

    [SpacetimeDB.Procedure]
    public static Result<PostHogConfigStatus, string> GetPosthogConfigStatus(ProcedureContext ctx) => PosthogTry(() =>
    {
        PosthogRequireProcedureAdmin(ctx);
        return ctx.WithTx(tx =>
        {
            var row = tx.Db.PosthogConfig.Singleton.Find(true);
            return new PostHogConfigStatus
            {
                IsConfigured = row is not null,
                Host = row?.Host,
                ProjectApiKeyLength = (uint)(row?.ProjectApiKey.Length ?? 0),
            };
        });
    });

    static PostHogConfig PosthogLoadConfig(ProcedureContext ctx) =>
        ctx.WithTx(tx => tx.Db.PosthogConfig.Singleton.Find(true)) ?? throw new Exception(PosthogErrors.ConfigMissing);

    // -----------------------------------------------------------------------
    // HTTP
    // -----------------------------------------------------------------------

    static PosthogHttpResult PosthogFetch(ProcedureContext ctx, PostHogConfig cfg, string path, JsonNode body)
    {
        var request = new HttpRequest
        {
            Uri = cfg.Host + path,
            Method = HttpMethod.Post,
            Headers = new() { new("content-type", "application/json") },
            Body = HttpBody.FromString(PosthogJsJson(body)),
        };
        PosthogHttpResult? result = null;
        try
        {
            ctx.Http.Send(request).Match(
                response =>
                {
                    result = new PosthogHttpResult(PosthogIsOkStatus(response.StatusCode), response.StatusCode, response.Body.ToStringUtf8Lossy());
                    return 0;
                },
                e =>
                {
                    result = new PosthogHttpResult(false, 0, e.ToString());
                    return 0;
                });
        }
        catch (Exception e)
        {
            result = new PosthogHttpResult(false, 0, e.Message);
        }
        return result!.Value;
    }

    /// <summary>A flag's value from a /flags?v=2 body: its variant string, or its enabled boolean.</summary>
    public static JsonNode? PosthogFeatureFlagValue(string body, string key)
    {
        JsonNode? parsed;
        try { parsed = JsonNode.Parse(body); } catch (Exception) { return null; }
        if (parsed is not JsonObject o || o["flags"] is not JsonObject flags || !flags.ContainsKey(key)) return null;
        if (flags[key] is not JsonObject flag || flag["enabled"] is not JsonValue en || en.GetValueKind() is not (JsonValueKind.True or JsonValueKind.False))
        {
            return null;
        }
        return flag["variant"] is JsonValue v && v.GetValueKind() == JsonValueKind.String
            ? JsonValue.Create(v.GetValue<string>())
            : JsonValue.Create(en.GetValueKind() == JsonValueKind.True);
    }

    readonly record struct PosthogBatchEvent(string Uuid, Timestamp CreatedAt, string DistinctId, string Event, string? PropertiesJson);

    static JsonObject PosthogBatchBody(string projectApiKey, IEnumerable<PosthogBatchEvent> events)
    {
        var batch = new JsonArray();
        foreach (var e in events)
        {
            batch.Add((JsonNode)new JsonObject
            {
                ["uuid"] = JsonValue.Create(e.Uuid),
                ["timestamp"] = JsonValue.Create(PosthogIsoString(e.CreatedAt)),
                ["distinct_id"] = JsonValue.Create(e.DistinctId),
                ["event"] = JsonValue.Create(e.Event),
                ["properties"] = (e.PropertiesJson is null ? null : PosthogParseJsonObject(e.PropertiesJson)) ?? new JsonObject(),
            });
        }
        return new JsonObject { ["api_key"] = JsonValue.Create(projectApiKey), ["batch"] = batch };
    }

    static void PosthogLogDelivery(Local db, Timestamp now, PostHogDeliverySource source, string? outboxId, string distinctId, string ev, PosthogHttpResult result)
    {
        var body = PosthogTruncateForLog(result.ResponseBody);
        db.PosthogDeliveryLog.Insert(new PostHogDeliveryLog
        {
            DeliveryId = 0,
            Source = source,
            OutboxId = outboxId,
            DistinctId = distinctId,
            Event = ev,
            Ok = result.Ok,
            StatusCode = result.StatusCode,
            ResponseBody = body,
            ErrorMessage = result.Ok ? null : body,
            AttemptedAt = now,
            AttemptedAtOrder = -now.MicrosecondsSinceUnixEpoch,
        });
    }

    // -----------------------------------------------------------------------
    // The outbox
    // -----------------------------------------------------------------------

    static string? PosthogEventInputError(string distinctId, string ev, string? propertiesJson)
    {
        var d = distinctId.Trim();
        var e = ev.Trim();
        if (d.Length == 0 || d.Length > PosthogMaxDistinctIdLength) return PosthogErrors.InvalidDistinctId;
        if (e.Length == 0 || e.Length > PosthogMaxEventNameLength) return PosthogErrors.InvalidEvent;
        if (propertiesJson is not null)
        {
            if (propertiesJson.Length > PosthogMaxPropertiesJsonLength) return PosthogErrors.PropertiesTooLarge;
            if (PosthogParseJsonObject(propertiesJson) is null) return PosthogErrors.InvalidPropertiesJson;
        }
        return null;
    }

    /// <summary>
    /// Queue an event in the caller's transaction. Never throws, so analytics can't roll back your
    /// reducer: invalid input is stored as Rejected with the code in LastError (and returned as Error).
    /// An idempotency key that's already queued is a no-op.
    /// </summary>
    public static PosthogEnqueueEventResult PosthogEnqueueEventInTx(ReducerContext ctx, PosthogEnqueueEventArgs args) =>
        PosthogEnqueueEventInTx(ctx.Db, ctx.Timestamp, ctx.NewUuidV7().ToString(), args);

    /// <summary>The same, given a transaction's db and time and a fresh UUID v7 string.</summary>
    public static PosthogEnqueueEventResult PosthogEnqueueEventInTx(Local db, Timestamp now, string uuid, PosthogEnqueueEventArgs args)
    {
        var key = args.IdempotencyKey?.Trim() is { Length: > 0 } k ? k : null;
        var error = PosthogEventInputError(args.DistinctId, args.Event, args.PropertiesJson)
            ?? (key is not null && key.Length > PosthogMaxIdempotencyKeyLength ? PosthogErrors.IdempotencyKeyTooLong : null);
        var outboxId = key is not null && error is null ? $"idem:{key}" : $"evt:{uuid}";
        if (db.PosthogOutbox.OutboxId.Find(outboxId) is not null) return new PosthogEnqueueEventResult(outboxId, false, null);
        var status = error is null ? PostHogOutboxStatus.Queued : PostHogOutboxStatus.Rejected;
        db.PosthogOutbox.Insert(new PostHogOutbox
        {
            OutboxId = outboxId,
            Uuid = uuid,
            IdempotencyKey = error is null ? key : null,
            DistinctId = PosthogSlice(args.DistinctId.Trim(), PosthogMaxDistinctIdLength),
            Event = PosthogSlice(args.Event.Trim(), PosthogMaxEventNameLength),
            PropertiesJson = error is null ? args.PropertiesJson : null,
            Status = status,
            Attempts = 0,
            ClaimId = null,
            ClaimExpiresAt = PosthogEpoch,
            NextAttemptAt = now,
            LastStatusCode = null,
            LastError = error,
            CreatedAt = now,
            UpdatedAt = now,
            DeliveredAt = null,
        });
        var delta = new PosthogStatsDelta();
        delta.Change(null, status);
        delta.Apply(db, now);
        return new PosthogEnqueueEventResult(outboxId, true, error);
    }

    /// <summary>Delete finished outbox rows and log rows last updated 30+ days before now: up to 1,000 per call.</summary>
    public static void PosthogPruneDeliveryHistory(Local db, Timestamp now)
    {
        var cutoff = PosthogPlus(now, -PosthogHistoryRetentionMicros);
        var delta = new PosthogStatsDelta();
        var budget = PosthogMaxRetentionRowsPerTick;
        foreach (var status in new[] { PostHogOutboxStatus.Delivered, PostHogOutboxStatus.Failed, PostHogOutboxStatus.Rejected })
        {
            var rows = db.PosthogOutbox.ByStatusUpdatedAt.Filter((status, new Bound<Timestamp>(PosthogMinTime, cutoff))).Take(budget).ToList();
            budget -= rows.Count;
            foreach (var row in rows)
            {
                db.PosthogOutbox.OutboxId.Delete(row.OutboxId);
                delta.Change(row.Status, null);
            }
        }
        foreach (var id in db.PosthogDeliveryLog.AttemptedAt.Filter(new Bound<Timestamp>(PosthogMinTime, cutoff)).Take(budget).Select(r => r.DeliveryId).ToList())
        {
            db.PosthogDeliveryLog.DeliveryId.Delete(id);
        }
        delta.Apply(db, now);
    }

    static List<PostHogOutbox> PosthogClaimQueuedRows(Local db, Timestamp now, int limit, string claimId)
    {
        var expired = db.PosthogOutbox.ByStatusClaimExpiresAt.Filter((PostHogOutboxStatus.Processing, new Bound<Timestamp>(PosthogMinTime, now)))
            .Take(PosthogMaxExpiredClaimsPerFlush).ToList();
        foreach (var row in expired)
        {
            if (row.ClaimExpiresAt.MicrosecondsSinceUnixEpoch > now.MicrosecondsSinceUnixEpoch) continue;
            db.PosthogOutbox.OutboxId.Update(row with
            {
                Status = PostHogOutboxStatus.Queued, ClaimId = null, ClaimExpiresAt = PosthogEpoch, NextAttemptAt = now, UpdatedAt = now,
            });
        }
        var due = db.PosthogOutbox.ByStatusNextAttemptAt.Filter((PostHogOutboxStatus.Queued, new Bound<Timestamp>(PosthogMinTime, now))).Take(limit).ToList();
        var expiresAt = PosthogPlus(now, PosthogClaimTtlMicros);
        return due.Select(row =>
        {
            var claimed = row with { Status = PostHogOutboxStatus.Processing, ClaimId = claimId, ClaimExpiresAt = expiresAt, UpdatedAt = now };
            db.PosthogOutbox.OutboxId.Update(claimed);
            return claimed;
        }).ToList();
    }

    static PostHogFlushResult PosthogDeliverOutbox(ProcedureContext ctx, PostHogConfig cfg, int limit)
    {
        var claimId = ctx.NewUuidV7().ToString();
        var rows = ctx.WithTx(tx => PosthogClaimQueuedRows(tx.Db, tx.Timestamp, limit, claimId));
        if (rows.Count == 0) return new PostHogFlushResult();
        var result = PosthogFetch(ctx, cfg, "/batch",
            PosthogBatchBody(cfg.ProjectApiKey, rows.Select(r => new PosthogBatchEvent(r.Uuid, r.CreatedAt, r.DistinctId, r.Event, r.PropertiesJson))));
        var now = ctx.Timestamp;
        return ctx.WithTx(tx =>
        {
            uint delivered = 0, failed = 0;
            var delta = new PosthogStatsDelta();
            foreach (var row in rows)
            {
                if (tx.Db.PosthogOutbox.OutboxId.Find(row.OutboxId) is not { } current) continue;
                if (current.Status != PostHogOutboxStatus.Processing || current.ClaimId != claimId) continue;
                PosthogLogDelivery(tx.Db, tx.Timestamp, PostHogDeliverySource.Flush, row.OutboxId, row.DistinctId, row.Event, result);
                var attempts = current.Attempts + 1;
                var terminal = result.Ok || PosthogIsPermanentFailure(result.StatusCode) || attempts >= PosthogMaxDeliveryAttempts;
                var status = result.Ok ? PostHogOutboxStatus.Delivered : terminal ? PostHogOutboxStatus.Failed : PostHogOutboxStatus.Queued;
                tx.Db.PosthogOutbox.OutboxId.Update(current with
                {
                    Status = status,
                    Attempts = attempts,
                    ClaimId = null,
                    ClaimExpiresAt = PosthogEpoch,
                    NextAttemptAt = terminal ? now : PosthogPlus(now, PosthogRetryDelayMicros(current.Attempts + 1)),
                    LastStatusCode = result.StatusCode,
                    LastError = result.Ok ? null : PosthogTruncateForLog(result.ResponseBody),
                    UpdatedAt = now,
                    DeliveredAt = result.Ok ? now : null,
                });
                delta.Change(current.Status, status);
                if (result.Ok) delivered++;
                else failed++;
            }
            delta.Apply(tx.Db, tx.Timestamp);
            return new PostHogFlushResult { Attempted = (uint)rows.Count, Delivered = delivered, Failed = failed };
        });
    }

    /// <summary>Runs every 5 s: prunes history, then sends up to 10 batches of 100 while full batches succeed. Scheduler only.</summary>
    [SpacetimeDB.Procedure]
    public static void PosthogScheduledFlush(ProcedureContext ctx, PostHogFlushTick tick)
    {
        if (ctx.Sender != ProcedureContextBase.Identity) throw new Exception(PosthogErrors.NotAuthorized);
        var cfg = ctx.WithTx(tx =>
        {
            PosthogPruneDeliveryHistory(tx.Db, tx.Timestamp);
            return tx.Db.PosthogConfig.Singleton.Find(true);
        });
        if (cfg is not { } c) return;
        for (var batch = 0; batch < PosthogMaxFlushBatchesPerTick; batch++)
        {
            if (PosthogDeliverOutbox(ctx, c, (int)PosthogMaxFlushLimit).Delivered < PosthogMaxFlushLimit) break;
        }
    }

    /// <summary>Admins only: queue an event, failing on invalid input.</summary>
    [SpacetimeDB.Reducer]
    public static void PosthogEnqueueEvent(ReducerContext ctx, string distinctId, string @event, string? propertiesJson, string? idempotencyKey)
    {
        PosthogRequireAdmin(ctx.Db, ctx.Sender);
        var r = PosthogEnqueueEventInTx(ctx, new PosthogEnqueueEventArgs
        {
            DistinctId = distinctId, Event = @event, PropertiesJson = propertiesJson, IdempotencyKey = idempotencyKey,
        });
        if (r.Error is not null) throw new Exception(r.Error);
    }

    /// <summary>Admins only: send one event now.</summary>
    [SpacetimeDB.Procedure]
    public static Result<PostHogCaptureResult, string> PosthogCaptureNow(ProcedureContext ctx, string distinctId, string @event, string? propertiesJson) => PosthogTry(() =>
    {
        PosthogRequireProcedureAdmin(ctx);
        if (PosthogEventInputError(distinctId, @event, propertiesJson) is { } e) throw new Exception(e);
        var cfg = PosthogLoadConfig(ctx);
        var uuid = ctx.NewUuidV7().ToString();
        var result = PosthogFetch(ctx, cfg, "/batch",
            PosthogBatchBody(cfg.ProjectApiKey, [new PosthogBatchEvent(uuid, ctx.Timestamp, distinctId, @event, propertiesJson)]));
        ctx.WithTx(tx =>
        {
            PosthogLogDelivery(tx.Db, tx.Timestamp, PostHogDeliverySource.Direct, null, distinctId, @event, result);
            return 0;
        });
        return new PostHogCaptureResult
        {
            Ok = result.Ok,
            StatusCode = result.StatusCode,
            Error = result.Ok ? null : PosthogTruncateForLog(result.ResponseBody),
        };
    });

    /// <summary>Admins only: send up to limit (1-100) queued events now.</summary>
    [SpacetimeDB.Procedure]
    public static Result<PostHogFlushResult, string> PosthogFlushOutbox(ProcedureContext ctx, uint limit) => PosthogTry(() =>
    {
        PosthogRequireProcedureAdmin(ctx);
        if (limit == 0 || limit > PosthogMaxFlushLimit) throw new Exception(PosthogErrors.InvalidFlushLimit);
        return PosthogDeliverOutbox(ctx, PosthogLoadConfig(ctx), (int)limit);
    });

    /// <summary>Admins only: move up to limit Failed events back to Queued with a fresh retry budget.</summary>
    [SpacetimeDB.Reducer]
    public static void PosthogRequeueFailedEvents(ReducerContext ctx, uint limit)
    {
        PosthogRequireAdmin(ctx.Db, ctx.Sender);
        if (limit == 0 || limit > PosthogMaxAdminBatch) throw new Exception(PosthogErrors.InvalidRequeueLimit);
        var delta = new PosthogStatsDelta();
        foreach (var row in ctx.Db.PosthogOutbox.Status.Filter(PostHogOutboxStatus.Failed).Take((int)limit).ToList())
        {
            ctx.Db.PosthogOutbox.OutboxId.Update(row with
            {
                Status = PostHogOutboxStatus.Queued, Attempts = 0, ClaimId = null, ClaimExpiresAt = PosthogEpoch,
                NextAttemptAt = ctx.Timestamp, UpdatedAt = ctx.Timestamp,
            });
            delta.Change(row.Status, PostHogOutboxStatus.Queued);
        }
        delta.Apply(ctx.Db, ctx.Timestamp);
    }

    /// <summary>Admins only: delete up to maxRows outbox and delivery log rows (PostHog keeps what it got).</summary>
    [SpacetimeDB.Reducer]
    public static void PosthogClearAnalytics(ReducerContext ctx, uint maxRows)
    {
        PosthogRequireAdmin(ctx.Db, ctx.Sender);
        if (maxRows == 0 || maxRows > PosthogMaxAdminBatch) throw new Exception(PosthogErrors.InvalidClearBatch);
        var delta = new PosthogStatsDelta();
        var outbox = ctx.Db.PosthogOutbox.Iter().Take((int)maxRows).ToList();
        foreach (var row in outbox)
        {
            ctx.Db.PosthogOutbox.OutboxId.Delete(row.OutboxId);
            delta.Change(row.Status, null);
        }
        foreach (var id in ctx.Db.PosthogDeliveryLog.Iter().Take((int)maxRows - outbox.Count).Select(r => r.DeliveryId).ToList())
        {
            ctx.Db.PosthogDeliveryLog.DeliveryId.Delete(id);
        }
        delta.Apply(ctx.Db, ctx.Timestamp);
    }

    static JsonObject? PosthogOptionalJsonObject(string? json, string tooLarge, string invalid)
    {
        if (json is null) return null;
        if (json.Length > PosthogMaxPropertiesJsonLength) throw new Exception(tooLarge);
        return PosthogParseJsonObject(json) ?? throw new Exception(invalid);
    }

    /// <summary>Admins only: evaluate a flag with PostHog /flags?v=2.</summary>
    [SpacetimeDB.Procedure]
    public static Result<PostHogFeatureFlagResult, string> PosthogGetFeatureFlag(ProcedureContext ctx, string key, string distinctId, string? personPropertiesJson, string? groupsJson) => PosthogTry(() =>
    {
        PosthogRequireProcedureAdmin(ctx);
        if (key.Trim().Length == 0 || key.Length > PosthogMaxEventNameLength) throw new Exception(PosthogErrors.InvalidFlagKey);
        if (distinctId.Trim().Length == 0 || distinctId.Length > PosthogMaxDistinctIdLength) throw new Exception(PosthogErrors.InvalidDistinctId);
        var person = PosthogOptionalJsonObject(personPropertiesJson, PosthogErrors.PersonPropertiesTooLarge, PosthogErrors.InvalidPersonPropertiesJson);
        var groups = PosthogOptionalJsonObject(groupsJson, PosthogErrors.GroupsTooLarge, PosthogErrors.InvalidGroupsJson);
        var cfg = PosthogLoadConfig(ctx);
        var body = new JsonObject { ["api_key"] = JsonValue.Create(cfg.ProjectApiKey), ["distinct_id"] = JsonValue.Create(distinctId) };
        if (person is not null) body["person_properties"] = person;
        if (groups is not null) body["groups"] = groups;
        var result = PosthogFetch(ctx, cfg, "/flags?v=2", body);
        var value = result.Ok ? PosthogFeatureFlagValue(result.ResponseBody, key) : null;
        ctx.WithTx(tx =>
        {
            PosthogLogDelivery(tx.Db, tx.Timestamp, PostHogDeliverySource.FeatureFlag, null, distinctId, $"$feature_flag:{key}", result);
            return 0;
        });
        var isFalse = value is JsonValue fv && fv.GetValueKind() == JsonValueKind.False;
        return new PostHogFeatureFlagResult
        {
            Ok = result.Ok,
            StatusCode = result.StatusCode,
            Enabled = value is null ? null : !isFalse,
            Variant = value is JsonValue sv && sv.GetValueKind() == JsonValueKind.String ? sv.GetValue<string>() : null,
            Error = result.Ok ? null : PosthogTruncateForLog(result.ResponseBody),
        };
    });

    // -----------------------------------------------------------------------
    // Admin views
    // -----------------------------------------------------------------------

    /// <summary>Up to 500 in-flight and queued events (admins only; empty for everyone else).</summary>
    [SpacetimeDB.View(Accessor = "PosthogOutboxAdmin", Public = true)]
    public static List<PostHogOutbox> PosthogOutboxAdmin(ViewContext ctx)
    {
        if (ctx.Db.PosthogAdminIdentity.Identity.Find(ctx.Sender) is null) return [];
        var rows = ctx.Db.PosthogOutbox.Status.Filter(PostHogOutboxStatus.Processing).Take(500).ToList();
        if (rows.Count < 500) rows.AddRange(ctx.Db.PosthogOutbox.Status.Filter(PostHogOutboxStatus.Queued).Take(500 - rows.Count));
        return rows;
    }

    /// <summary>The 50 most recent delivery attempts (admins only).</summary>
    [SpacetimeDB.View(Accessor = "PosthogDeliveryLogAdmin", Public = true)]
    public static List<PostHogDeliveryLogRow> PosthogDeliveryLogAdmin(ViewContext ctx)
    {
        if (ctx.Db.PosthogAdminIdentity.Identity.Find(ctx.Sender) is null) return [];
        return ctx.Db.PosthogDeliveryLog.AttemptedAtOrder.Filter(new Bound<long>(long.MinValue, long.MaxValue)).Take(50)
            .Select(r => new PostHogDeliveryLogRow
            {
                DeliveryId = r.DeliveryId,
                Source = r.Source,
                OutboxId = r.OutboxId,
                DistinctId = r.DistinctId,
                Event = r.Event,
                Ok = r.Ok,
                StatusCode = r.StatusCode,
                ResponseBody = r.ResponseBody,
                ErrorMessage = r.ErrorMessage,
                AttemptedAt = r.AttemptedAt,
            })
            .ToList();
    }
}
