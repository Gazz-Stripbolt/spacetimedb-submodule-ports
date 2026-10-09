// Stripe.cs: Stripe payments, Checkout, subscriptions and webhooks, for C# modules.
//
// Unofficial C# port of @spacetimedb/stripe (SpacetimeDB PR #5906, commit
// 16396207cea1a07f4ac476864a05ffcfd19bfc25, package spacetime-stripe-ts). Licensed Apache-2.0,
// like the original. Original design and behaviour by Clockwork Labs; this port is by Tinker
// (Gazz-Stripbolt).
//
// Mirrors customers, subscriptions, Checkout sessions, invoices and payments into private tables
// from signed Stripe webhooks, with the same out-of-order protection, admin-gated billing
// procedures, host helpers, error codes and limits as the TS package. Needs Crypto.cs from this
// repo (Stripe signatures).
//
//     public static partial class Module
//     {
//         [Reducer(ReducerKind.Init)]
//         public static void Init(ReducerContext ctx) => StripeInstall(ctx);
//
//         [HttpRouter]
//         public static Router Routes() => Router.New().Post("/stripe/webhook", Handlers.StripeWebhook);
//
//         // A host procedure resolves the user from ctx.Sender and takes prices from its own catalog:
//         //   StripeCreateUserCheckoutSession(ctx, new StripeUserCheckoutArgs { UserId = …, Items = …, Mode = "payment", … });
//     }
//
// Changes from the TypeScript original:
// - Names: generic procedures and reducers get a stripe_ prefix (get_customer →
//   stripe_get_customer, replay_webhook_event → stripe_replay_webhook_event, prune_webhook_events
//   → stripe_prune_webhook_events, …) and generic types a Stripe prefix (WebhookEventStatus →
//   StripeWebhookEventStatus, CheckoutSessionResult → StripeCheckoutSessionResult, …).
// - ValidateStripePriceResult.type is price_type (the Rust port can't name a field "type").
// - Procedures return Result<T, string> instead of throwing.
// - Lookups by user, org, or an optional customer/invoice id scan the table (the Rust port can't
//   filter an index on an optional column; both behave the same). Org and user subscription lists
//   are sorted by inserted_at, like the TS composite indexes.
// - stripe_prune_webhook_events rejects calls that don't come from the scheduler.
// - Payload validation is hand-written: Failed rows carry differently worded messages. A
//   non-integer or out-of-range number where TS calls BigInt() aborts the transaction.
// - The API origin can be overridden with the optional partial method StripeOverrideApiBase.

#pragma warning disable STDB_UNSTABLE
#nullable enable

using System.Globalization;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using SpacetimeDB;
using HttpMethod = SpacetimeDB.HttpMethod;

[SpacetimeDB.Type]
public enum StripeWebhookEventStatus
{
    Received,
    Processed,
    Ignored,
    Failed,
}

[SpacetimeDB.Type]
public partial struct StripeCheckoutSessionResult
{
    public string SessionId;
    public string? Url;
}

[SpacetimeDB.Type]
public partial struct StripeGetOrCreateCustomerResult
{
    public string CustomerId;
    public bool IsNew;
}

[SpacetimeDB.Type]
public partial struct StripePortalSessionResult
{
    public string Url;
}

[SpacetimeDB.Type]
public partial struct StripeConfigStatus
{
    public bool IsConfigured;
    public bool HasWebhookSecret;
    public string? StripeVersion;
    public ushort SecretKeyLength;
}

[SpacetimeDB.Type]
public partial struct ValidateStripePriceResult
{
    public bool Valid;
    public ushort Status;
    public bool? Active;
    public string? Currency;
    public long? UnitAmount;
    public bool? Livemode;
    /// <summary>TS type (see the header).</summary>
    public string? PriceType;
    public string? Message;
    public string? Code;
    public string? ErrorType;
}

[SpacetimeDB.Type]
public partial struct StripeRemoteCheckoutSessionResult
{
    public bool Ok;
    public ushort Status;
    public string? SessionId;
    public string? PaymentStatus;
    public string? SessionStatus;
    public string? Mode;
    public long? AmountTotal;
    public string? Currency;
    public string? CustomerId;
    public string? PaymentIntentId;
    public string? Message;
    public string? Code;
    public string? ErrorType;
}

[SpacetimeDB.Type]
public partial struct StripeCheckoutLineItem
{
    public string PriceId;
    public long Quantity;
}

/// <summary>A Stripe API response.</summary>
public readonly record struct StripeResponse(ushort Status, string Body);

/// <summary>Arguments for StripeCreateCheckoutSessionInStripe (the TS CheckoutSessionArgs).</summary>
public sealed class StripeCheckoutSessionArgs
{
    public List<StripeCheckoutLineItem> Items { get; init; } = [];
    public string? CustomerId { get; init; }
    public string Mode { get; init; } = "payment";
    public string SuccessUrl { get; init; } = "";
    public string CancelUrl { get; init; } = "";
    public string? MetadataJson { get; init; }
    public string? SubscriptionMetadataJson { get; init; }
    public string? PaymentIntentMetadataJson { get; init; }
}

/// <summary>Arguments for StripeCreateUserCheckoutSession.</summary>
public sealed class StripeUserCheckoutArgs
{
    public string UserId { get; init; } = "";
    public string? Email { get; init; }
    public string? Name { get; init; }
    public List<StripeCheckoutLineItem> Items { get; init; } = [];
    /// <summary>payment or subscription.</summary>
    public string Mode { get; init; } = "payment";
    public string SuccessUrl { get; init; } = "";
    public string CancelUrl { get; init; } = "";
    public List<(string Key, string Value)> Metadata { get; init; } = [];
}

/// <summary>Thrown where the TS code would throw mid-transaction (e.g. BigInt of a non-integer): aborts it.</summary>
public sealed class StripeAbortException(string message) : Exception(message);

/// <summary>A payload that doesn't match the TS schemas (valibot): recorded as Failed.</summary>
public sealed class StripeIssueException(string message) : Exception(message);

public static partial class Module
{
    public static class StripeErrors
    {
        public const string NotAuthorized = "stripe.not_authorized";
        public const string CannotRemoveLastAdmin = "stripe.cannot_remove_last_admin";
        public const string ConfigNotSet = "stripe.config_not_set";
        public const string InvalidWebhookSigningSecret = "stripe.invalid_webhook_signing_secret";
        public const string RequestInvalid = "stripe.request_invalid";
        public const string RequestPathInvalid = "stripe.request_path_invalid";
        public const string RequestMethodInvalid = "stripe.request_method_invalid";
        public const string RequestBodyTooLarge = "stripe.request_body_too_large";
        public const string IdempotencyKeyTooLong = "stripe.idempotency_key_too_long";
        public const string CreateCustomerFailed = "stripe.create_customer_failed";
        public const string CreateCustomerInvalidResponse = "stripe.create_customer_invalid_response";
        public const string CheckoutSessionRequiresItems = "stripe.checkout_session_requires_items";
        public const string CheckoutSessionFailed = "stripe.checkout_session_failed";
        public const string CheckoutSessionInvalidResponse = "stripe.checkout_session_invalid_response";
        public const string PortalSessionFailed = "stripe.portal_session_failed";
        public const string PortalSessionInvalidResponse = "stripe.portal_session_invalid_response";
        public const string SubscriptionNotFound = "stripe.subscription_not_found";
        public const string SubscriptionUpdateFailed = "stripe.subscription_update_failed";
        public const string SubscriptionUpdateInvalidResponse = "stripe.subscription_update_invalid_response";
        public const string SubscriptionCancelFailed = "stripe.subscription_cancel_failed";
        public const string SubscriptionCancelInvalidResponse = "stripe.subscription_cancel_invalid_response";
        public const string SubscriptionLookupFailed = "stripe.subscription_lookup_failed";
        public const string SubscriptionLookupInvalidResponse = "stripe.subscription_lookup_invalid_response";
        public const string SubscriptionMissingLineItems = "stripe.subscription_missing_line_items";
        public const string SubscriptionItemUpdateFailed = "stripe.subscription_item_update_failed";
        public const string SubscriptionPayloadMissingFields = "stripe.subscription_payload_missing_fields";
        public const string WebhookMetadataInvalid = "stripe.webhook_metadata_invalid";
        public const string WebhookPayloadTooLarge = "stripe.webhook_payload_too_large";
        public const string WebhookSignatureTooLarge = "stripe.webhook_signature_too_large";
        public const string WebhookSecretNotConfigured = "stripe.webhook_secret_not_configured";
        public const string WebhookSignatureMismatch = "stripe.webhook_signature_mismatch";
        public const string WebhookPayloadMissingMetadata = "stripe.webhook_payload_missing_metadata";
        public const string WebhookMetadataMismatch = "stripe.webhook_metadata_mismatch";
        public const string WebhookEventNotFound = "stripe.webhook_event_not_found";
    }

    public const int StripeMaxWebhookBodyLength = 1024 * 1024;
    public const int StripeMaxWebhookHeaderLength = 8192;
    public const int StripeMaxWebhookMetadataLength = 255;
    public const long StripeWebhookEventRetentionMicros = 30L * 24 * 60 * 60 * 1_000_000;
    public const long StripeWebhookPruneIntervalMicros = 60L * 60 * 1_000_000;
    public const int StripeWebhookPruneBatch = 500;
    const int StripeMaxPathLength = 2048;
    const int StripeMaxFormBodyLength = 64 * 1024;
    const int StripeMaxIdempotencyKeyLength = 255;
    const int StripeMaxQueryRows = 1000;

    /// <summary>Optional: implement to point API calls somewhere else (tests use a mock).</summary>
    static partial void StripeOverrideApiBase(ref string apiBase);

    static string StripeApiOrigin()
    {
        var b = "https://api.stripe.com";
        StripeOverrideApiBase(ref b);
        return b;
    }

    // -----------------------------------------------------------------------
    // Tables
    // -----------------------------------------------------------------------

    [SpacetimeDB.Table(Accessor = "StripeCustomer")]
    public partial struct StripeCustomer
    {
        [SpacetimeDB.PrimaryKey]
        public string StripeCustomerId;
        public string? AppUserId;
        public string? Email;
        public string? Name;
        public string? MetadataJson;
        [SpacetimeDB.Index.BTree]
        public string? UserId;
        public long EventCreatedUnix;
        public Timestamp CreatedAt;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "StripeSubscription")]
    [SpacetimeDB.Index.BTree(Accessor = "ByOrgInsertedAt", Columns = new[] { nameof(OrgId), nameof(InsertedAt) })]
    [SpacetimeDB.Index.BTree(Accessor = "ByUserInsertedAt", Columns = new[] { nameof(UserId), nameof(InsertedAt) })]
    public partial struct StripeSubscription
    {
        [SpacetimeDB.PrimaryKey]
        public string StripeSubscriptionId;
        [SpacetimeDB.Index.BTree]
        public string StripeCustomerId;
        public string Status;
        public long CurrentPeriodEndUnix;
        public bool CancelAtPeriodEnd;
        public long? CancelAtUnix;
        public long? Quantity;
        public string? PriceId;
        public string? MetadataJson;
        [SpacetimeDB.Index.BTree]
        public string? OrgId;
        [SpacetimeDB.Index.BTree]
        public string? UserId;
        public long EventCreatedUnix;
        public Timestamp InsertedAt;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "StripeCheckoutSession")]
    public partial struct StripeCheckoutSession
    {
        [SpacetimeDB.PrimaryKey]
        public string StripeCheckoutSessionId;
        [SpacetimeDB.Index.BTree]
        public string? StripeCustomerId;
        public string Status;
        public string PaymentStatus;
        public string Mode;
        public string? MetadataJson;
        public long EventCreatedUnix;
        public Timestamp InsertedAt;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "StripePayment")]
    public partial struct StripePayment
    {
        [SpacetimeDB.PrimaryKey]
        public string StripePaymentIntentId;
        [SpacetimeDB.Index.BTree]
        public string? StripeCustomerId;
        [SpacetimeDB.Index.BTree]
        public string? StripeInvoiceId;
        public long Amount;
        public string Currency;
        public string Status;
        public long CreatedUnix;
        public string? MetadataJson;
        [SpacetimeDB.Index.BTree]
        public string? OrgId;
        [SpacetimeDB.Index.BTree]
        public string? UserId;
        public Timestamp InsertedAt;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "StripeInvoice")]
    public partial struct StripeInvoice
    {
        [SpacetimeDB.PrimaryKey]
        public string StripeInvoiceId;
        [SpacetimeDB.Index.BTree]
        public string StripeCustomerId;
        [SpacetimeDB.Index.BTree]
        public string? StripeSubscriptionId;
        public string Status;
        public long AmountDue;
        public long AmountPaid;
        public long CreatedUnix;
        [SpacetimeDB.Index.BTree]
        public string? OrgId;
        [SpacetimeDB.Index.BTree]
        public string? UserId;
        public long EventCreatedUnix;
        public Timestamp InsertedAt;
        public Timestamp UpdatedAt;
    }

    /// <summary>Idempotency log with each event's payload (not its signature header).</summary>
    [SpacetimeDB.Table(Accessor = "StripeWebhookEvent")]
    public partial struct StripeWebhookEvent
    {
        [SpacetimeDB.PrimaryKey]
        public string EventId;
        public string EventType;
        public bool Livemode;
        public string PayloadJson;
        public StripeWebhookEventStatus Status;
        public string? ErrorMessage;
        [SpacetimeDB.Index.BTree]
        public Timestamp ReceivedAt;
        public Timestamp? ProcessedAt;
    }

    /// <summary>Schedules the retention sweep of stripe_webhook_event.</summary>
    [SpacetimeDB.Table(Accessor = "StripeWebhookPruneTick", Scheduled = nameof(StripePruneWebhookEvents), ScheduledAt = nameof(StripeWebhookPruneTick.ScheduledAt))]
    public partial struct StripeWebhookPruneTick
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
    }

    [SpacetimeDB.Table(Accessor = "StripeConfig")]
    public partial struct StripeConfig
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public string SecretKey;
        public string? StripeVersion;
        public string? WebhookSigningSecret;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "StripeAdminIdentity")]
    public partial struct StripeAdminIdentity
    {
        [SpacetimeDB.PrimaryKey]
        public Identity Identity;
        public Timestamp AddedAt;
    }

    // -----------------------------------------------------------------------
    // JS-compatible JSON and form helpers
    // -----------------------------------------------------------------------

    static bool StripeHasControlCharacter(string s)
    {
        foreach (var c in s) if (c <= 0x1f || c == 0x7f) return true;
        return false;
    }

    static string StripeCollapseWhitespace(string s)
    {
        var sb = new StringBuilder(s.Length);
        var inWs = false;
        foreach (var c in s)
        {
            if (char.IsWhiteSpace(c) || c == '﻿')
            {
                if (!inWs) sb.Append(' ');
                inWs = true;
            }
            else
            {
                sb.Append(c);
                inWs = false;
            }
        }
        return sb.ToString();
    }

    static string StripeSlice(string s, int n)
    {
        if (s.Length <= n) return s;
        if (n > 0 && char.IsHighSurrogate(s[n - 1])) n--;
        return s[..n];
    }

    static bool StripeIsArrayIndex(string k) =>
        k.Length > 0 && k.All(c => c is >= '0' and <= '9') && (k == "0" || k[0] != '0')
        && ulong.TryParse(k, NumberStyles.None, CultureInfo.InvariantCulture, out var n) && n < 4_294_967_295;

    /// <summary>Object.entries order: integer-like keys first, ascending.</summary>
    static List<KeyValuePair<string, JsonNode?>> StripeJsEntries(JsonObject o)
    {
        var index = o.Where(kv => StripeIsArrayIndex(kv.Key)).OrderBy(kv => ulong.Parse(kv.Key, CultureInfo.InvariantCulture)).ToList();
        index.AddRange(o.Where(kv => !StripeIsArrayIndex(kv.Key)));
        return index;
    }

    static string StripeJsNumber(JsonValue v)
    {
        if (v.TryGetValue<long>(out var l)) return l.ToString(CultureInfo.InvariantCulture);
        var d = v.GetValue<double>();
        if (Math.Abs(d) < 1e21 && Math.Floor(d) == d) return d.ToString("F0", CultureInfo.InvariantCulture);
        return d.ToString("R", CultureInfo.InvariantCulture);
    }

    /// <summary>A JSON string literal escaped as JSON.stringify does (non-ASCII kept raw).</summary>
    public static string StripeJsString(string s)
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

    /// <summary>JSON.stringify(JSON.parse(…)) as JS writes it.</summary>
    public static string StripeJsJson(JsonNode? v) => v switch
    {
        null => "null",
        JsonObject o => "{" + string.Join(",", StripeJsEntries(o).Select(kv => StripeJsString(kv.Key) + ":" + StripeJsJson(kv.Value))) + "}",
        JsonArray a => "[" + string.Join(",", a.Select(StripeJsJson)) + "]",
        JsonValue x => x.GetValueKind() switch
        {
            JsonValueKind.String => StripeJsString(x.GetValue<string>()),
            JsonValueKind.Number => StripeJsNumber(x),
            JsonValueKind.True => "true",
            JsonValueKind.False => "false",
            _ => "null",
        },
        _ => "null",
    };

    /// <summary>JS String(value).</summary>
    static string StripeJsStringOf(JsonNode? v) => v switch
    {
        null => "null",
        JsonArray a => string.Join(",", a.Select(i => i is null ? "" : StripeJsStringOf(i))),
        JsonObject => "[object Object]",
        JsonValue x => x.GetValueKind() switch
        {
            JsonValueKind.String => x.GetValue<string>(),
            JsonValueKind.Number => StripeJsNumber(x),
            JsonValueKind.True => "true",
            JsonValueKind.False => "false",
            _ => "null",
        },
        _ => "null",
    };

    /// <summary>JS encodeURIComponent.</summary>
    public static string StripeEncodeUriComponent(string s)
    {
        var sb = new StringBuilder(s.Length);
        foreach (var b in Encoding.UTF8.GetBytes(s))
        {
            if (b is >= (byte)'a' and <= (byte)'z' or >= (byte)'A' and <= (byte)'Z' or >= (byte)'0' and <= (byte)'9'
                || "-_.!~*'()".IndexOf((char)b) >= 0)
            {
                sb.Append((char)b);
            }
            else
            {
                sb.Append('%').Append(b.ToString("X2", CultureInfo.InvariantCulture));
            }
        }
        return sb.ToString();
    }

    public static string StripeFormPairsToBody(IEnumerable<(string Key, string? Value)> pairs) =>
        string.Join("&", pairs.Where(p => p.Value is not null).Select(p => $"{StripeEncodeUriComponent(p.Key)}={StripeEncodeUriComponent(p.Value!)}"));

    static JsonNode? StripeParse(string? s)
    {
        if (s is null) return null;
        try { return JsonNode.Parse(s); } catch (Exception) { return null; }
    }

    static bool StripeIsRecord(JsonNode? v) => v is JsonObject or JsonArray;

    static List<(string, string?)> StripeMetadataJsonToFormPairs(string prefix, string? metadataJson)
    {
        if (string.IsNullOrEmpty(metadataJson)) return [];
        var entries = StripeParse(metadataJson) switch
        {
            JsonObject o => StripeJsEntries(o).Select(kv => (kv.Key, kv.Value)).ToList(),
            JsonArray a => a.Select((v, i) => (i.ToString(CultureInfo.InvariantCulture), v)).ToList(),
            _ => [],
        };
        return entries.Where(e => e.Item2 is not null).Select(e => ($"{prefix}[{e.Item1}]", (string?)StripeJsStringOf(e.Item2))).ToList();
    }

    static string? StripeMaybeString(JsonNode? v) => v is JsonValue x && x.GetValueKind() == JsonValueKind.String ? x.GetValue<string>() : null;

    static bool? StripeMaybeBool(JsonNode? v) =>
        v is JsonValue x ? x.GetValueKind() switch { JsonValueKind.True => true, JsonValueKind.False => false, _ => null } : null;

    static long? StripeMaybeI64(JsonNode? v)
    {
        if (v is not JsonValue x || x.GetValueKind() != JsonValueKind.Number) return null;
        if (x.TryGetValue<long>(out var l)) return l;
        var d = x.GetValue<double>();
        return double.IsFinite(d) && Math.Floor(d) == d && Math.Abs(d) < 9.2e18 ? (long)d : null;
    }

    static string? StripeMaybeId(JsonNode? v) => v switch
    {
        JsonValue x when x.GetValueKind() == JsonValueKind.String => x.GetValue<string>(),
        JsonObject o => StripeMaybeString(o["id"]),
        _ => null,
    };

    static JsonNode? StripeGet(JsonNode? v, string key) => v is JsonObject o ? o[key] : null;

    static (string? Json, string? OrgId, string? UserId) StripeMetadataInfo(JsonNode? metadata) => metadata switch
    {
        JsonObject o => (StripeJsJson(o), StripeMaybeString(o["orgId"]), StripeMaybeString(o["userId"])),
        JsonArray a => (StripeJsJson(a), null, null),
        _ => (null, null, null),
    };

    static (string? Json, string? OrgId, string? UserId) StripeCoerceMetadataFromJson(string? metadataJson)
    {
        if (string.IsNullOrEmpty(metadataJson)) return (null, null, null);
        var (json, org, user) = StripeMetadataInfo(StripeParse(metadataJson));
        return (json ?? metadataJson, org, user);
    }

    static bool StripeDeriveCancelAtPeriodEnd(long? cancelAtUnix, long currentPeriodEndUnix) =>
        cancelAtUnix is { } c && currentPeriodEndUnix > 0 && Math.Abs(c - currentPeriodEndUnix) <= 5 * 60;

    /// <summary>:type=…|code=…|msg=…|log=… from a Stripe error body, else :body=….</summary>
    public static string StripeErrorSuffix(string body)
    {
        var parsed = StripeParse(body);
        if (StripeIsRecord(parsed) && StripeGet(parsed, "error") is { } e && StripeIsRecord(e))
        {
            var parts = new List<string>();
            if (StripeMaybeString(StripeGet(e, "type")) is { Length: > 0 } t) parts.Add($"type={t}");
            if (StripeMaybeString(StripeGet(e, "code")) is { Length: > 0 } c) parts.Add($"code={c}");
            if (StripeMaybeString(StripeGet(e, "message")) is { Length: > 0 } m) parts.Add($"msg={StripeSlice(StripeCollapseWhitespace(m), 240)}");
            if (StripeMaybeString(StripeGet(e, "request_log_url")) is { Length: > 0 } l) parts.Add($"log={l}");
            if (parts.Count > 0) return ":" + string.Join("|", parts);
        }
        var compact = StripeCollapseWhitespace(body).Trim();
        return compact.Length == 0 ? "" : $":body={StripeSlice(compact, 240)}";
    }

    static long StripeUnixSeconds(Timestamp t) => (long)Math.Floor(t.MicrosecondsSinceUnixEpoch / 1_000_000.0);

    // -----------------------------------------------------------------------
    // Admins and config
    // -----------------------------------------------------------------------

    /// <summary>Call from your Init reducer: seeds the publishing identity and the hourly retention sweep.</summary>
    public static void StripeInstall(ReducerContext ctx)
    {
        if (ctx.Db.StripeAdminIdentity.Identity.Find(ctx.Sender) is null)
        {
            ctx.Db.StripeAdminIdentity.Insert(new StripeAdminIdentity { Identity = ctx.Sender, AddedAt = ctx.Timestamp });
        }
        if (ctx.Db.StripeWebhookPruneTick.Count == 0)
        {
            ctx.Db.StripeWebhookPruneTick.Insert(new StripeWebhookPruneTick
            {
                ScheduledId = 0,
                ScheduledAt = new ScheduleAt.Interval(new TimeDuration(StripeWebhookPruneIntervalMicros)),
            });
        }
    }

    public static bool StripeIsAdmin(Local db, Identity who) => db.StripeAdminIdentity.Identity.Find(who) is not null;

    static bool StripeProcedureIsAdmin(ProcedureContext ctx)
    {
        var sender = ctx.Sender;
        return ctx.WithTx(tx => StripeIsAdmin(tx.Db, sender));
    }

    static Result<T, string> StripeErr<T>(string e) => Result<T, string>.Err(e);
    static Result<Unit, string> StripeUnitOk() => Result<Unit, string>.Ok(default);

    /// <summary>Admin check, then f; exceptions become Err(message).</summary>
    static Result<T, string> StripeAdmin<T>(ProcedureContext ctx, Func<T> f)
    {
        if (!StripeProcedureIsAdmin(ctx)) return StripeErr<T>(StripeErrors.NotAuthorized);
        try { return Result<T, string>.Ok(f()); }
        catch (Exception e) { return StripeErr<T>(e.Message); }
    }

    [SpacetimeDB.Procedure]
    public static Result<Unit, string> StripeAddAdminIdentity(ProcedureContext ctx, Identity identity) => StripeAdmin(ctx, () =>
    {
        var now = ctx.Timestamp;
        ctx.WithTx(tx =>
        {
            if (tx.Db.StripeAdminIdentity.Identity.Find(identity) is null)
            {
                tx.Db.StripeAdminIdentity.Insert(new StripeAdminIdentity { Identity = identity, AddedAt = now });
            }
            return 0;
        });
        return default(Unit);
    });

    [SpacetimeDB.Procedure]
    public static Result<Unit, string> StripeRemoveAdminIdentity(ProcedureContext ctx, Identity identity) => StripeAdmin(ctx, () =>
    {
        var error = ctx.WithTx(tx =>
        {
            if (tx.Db.StripeAdminIdentity.Identity.Find(identity) is null) return null;
            if (tx.Db.StripeAdminIdentity.Count <= 1) return StripeErrors.CannotRemoveLastAdmin;
            tx.Db.StripeAdminIdentity.Identity.Delete(identity);
            return (string?)null;
        });
        if (error is not null) throw new Exception(error);
        return default(Unit);
    });

    public static StripeConfig StripeLoadConfig(Local db) => db.StripeConfig.Singleton.Find(true) ?? throw new Exception(StripeErrors.ConfigNotSet);

    /// <summary>Admins only. null keeps the stored version / webhook secret.</summary>
    [SpacetimeDB.Procedure]
    public static Result<Unit, string> SetStripeConfig(ProcedureContext ctx, string secretKey, string? stripeVersion, string? webhookSigningSecret) => StripeAdmin(ctx, () =>
    {
        var now = ctx.Timestamp;
        ctx.WithTx(tx =>
        {
            var existing = tx.Db.StripeConfig.Singleton.Find(true);
            var row = new StripeConfig
            {
                Singleton = true,
                SecretKey = secretKey,
                StripeVersion = stripeVersion ?? existing?.StripeVersion,
                WebhookSigningSecret = webhookSigningSecret ?? existing?.WebhookSigningSecret,
                UpdatedAt = now,
            };
            if (existing is not null) tx.Db.StripeConfig.Singleton.Update(row);
            else tx.Db.StripeConfig.Insert(row);
            return 0;
        });
        return default(Unit);
    });

    /// <summary>Admins only: rotate just the webhook secret.</summary>
    [SpacetimeDB.Procedure]
    public static Result<Unit, string> SetStripeWebhookSigningSecret(ProcedureContext ctx, string webhookSigningSecret) => StripeAdmin(ctx, () =>
    {
        var secret = webhookSigningSecret.Trim();
        if (secret.Length == 0) throw new Exception(StripeErrors.InvalidWebhookSigningSecret);
        var now = ctx.Timestamp;
        var error = ctx.WithTx(tx =>
        {
            if (tx.Db.StripeConfig.Singleton.Find(true) is not { } existing) return StripeErrors.ConfigNotSet;
            tx.Db.StripeConfig.Singleton.Update(existing with { WebhookSigningSecret = secret, UpdatedAt = now });
            return (string?)null;
        });
        if (error is not null) throw new Exception(error);
        return default(Unit);
    });

    [SpacetimeDB.Procedure]
    public static Result<StripeConfigStatus, string> GetStripeConfigStatus(ProcedureContext ctx) => StripeAdmin(ctx, () =>
        ctx.WithTx(tx => tx.Db.StripeConfig.Singleton.Find(true) is { } row
            ? new StripeConfigStatus
            {
                IsConfigured = true,
                HasWebhookSecret = row.WebhookSigningSecret is not null,
                StripeVersion = row.StripeVersion,
                SecretKeyLength = (ushort)Math.Min(row.SecretKey.Length, ushort.MaxValue),
            }
            : new StripeConfigStatus { IsConfigured = false, HasWebhookSecret = false, StripeVersion = null, SecretKeyLength = 0 }));

    // -----------------------------------------------------------------------
    // HTTP
    // -----------------------------------------------------------------------

    /// <summary>A validated request (the TS buildStripeHttpRequest).</summary>
    public sealed record StripeHttpRequest(string Method, string Url, List<(string Name, string Value)> Headers, string? Body);

    public static StripeHttpRequest StripeBuildHttpRequest(string method, string path, string secretKey, string? stripeVersion, string? formBody, string? idempotencyKey)
    {
        method = method.Trim().ToUpperInvariant();
        if (method is not ("GET" or "POST" or "DELETE")) throw new Exception(StripeErrors.RequestMethodInvalid);
        path = path.Trim();
        if (!path.StartsWith("/v1/", StringComparison.Ordinal) || path.StartsWith("//", StringComparison.Ordinal) || path.Contains('\\')
            || path.Contains('#') || path.Length > StripeMaxPathLength || StripeHasControlCharacter(path))
        {
            throw new Exception(StripeErrors.RequestPathInvalid);
        }
        var body = string.IsNullOrEmpty(formBody) ? null : formBody;
        if (body is not null && body.Length > StripeMaxFormBodyLength) throw new Exception(StripeErrors.RequestBodyTooLarge);
        if (!string.IsNullOrEmpty(idempotencyKey) && idempotencyKey.Length > StripeMaxIdempotencyKeyLength)
        {
            throw new Exception(StripeErrors.IdempotencyKeyTooLong);
        }
        var headers = new List<(string, string)> { ("Authorization", $"Bearer {secretKey}") };
        if (!string.IsNullOrEmpty(stripeVersion)) headers.Add(("Stripe-Version", stripeVersion));
        if (!string.IsNullOrEmpty(idempotencyKey)) headers.Add(("Idempotency-Key", idempotencyKey));
        if (body is not null) headers.Add(("Content-Type", "application/x-www-form-urlencoded"));
        return new StripeHttpRequest(method, StripeApiOrigin() + path, headers, body);
    }

    static StripeResponse StripeCall(ProcedureContext ctx, StripeConfig cfg, string method, string path, string? formBody, string? idempotencyKey)
    {
        var r = StripeBuildHttpRequest(method, path, cfg.SecretKey, cfg.StripeVersion, formBody, idempotencyKey);
        var request = new HttpRequest
        {
            Uri = r.Url,
            Method = new HttpMethod(r.Method),
            Headers = r.Headers.Select(h => new HttpHeader(h.Name, h.Value)).ToList(),
            Body = r.Body is null ? HttpBody.Empty : HttpBody.FromString(r.Body),
        };
        StripeResponse? result = null;
        string? error = null;
        ctx.Http.Send(request).Match(
            response => { result = new StripeResponse(response.StatusCode, response.Body.ToStringUtf8Lossy()); return 0; },
            e => { error = e.ToString(); return 0; });
        return result ?? throw new Exception(error ?? StripeErrors.RequestInvalid);
    }

    static void StripeEnsureOk(StripeResponse r, string code)
    {
        if (r.Status is < 200 or >= 300) throw new Exception($"{code}:{r.Status}{StripeErrorSuffix(r.Body)}");
    }

    static StripeConfig StripeConfigFor(ProcedureContext ctx) => ctx.WithTx(tx => StripeLoadConfig(tx.Db));

    /// <summary>A request to a relative /v1/ path with the stored secret key. No authorization.</summary>
    public static StripeResponse StripeRequest(ProcedureContext ctx, string method, string path, string? formBody = null, string? idempotencyKey = null) =>
        StripeCall(ctx, StripeConfigFor(ctx), method, path, formBody, idempotencyKey);

    // -----------------------------------------------------------------------
    // Row upserts (sparse: null keeps the stored value where TS uses ??)
    // -----------------------------------------------------------------------

    static void StripeUpsertCustomer(Local db, Timestamp now, StripeCustomer row)
    {
        var existing = db.StripeCustomer.StripeCustomerId.Find(row.StripeCustomerId);
        if (existing is { } e)
        {
            row.AppUserId ??= e.AppUserId;
            row.Email ??= e.Email;
            row.Name ??= e.Name;
            row.MetadataJson ??= e.MetadataJson;
            row.UserId ??= e.UserId;
            row.CreatedAt = e.CreatedAt;
        }
        else row.CreatedAt = now;
        row.UpdatedAt = now;
        if (existing is not null) db.StripeCustomer.StripeCustomerId.Update(row);
        else db.StripeCustomer.Insert(row);
    }

    static void StripeUpsertSubscription(Local db, Timestamp now, StripeSubscription row)
    {
        var existing = db.StripeSubscription.StripeSubscriptionId.Find(row.StripeSubscriptionId);
        if (existing is { } e)
        {
            row.PriceId ??= e.PriceId;
            row.MetadataJson ??= e.MetadataJson;
            row.OrgId ??= e.OrgId;
            row.UserId ??= e.UserId;
            row.InsertedAt = e.InsertedAt;
        }
        else row.InsertedAt = now;
        row.UpdatedAt = now;
        if (existing is not null) db.StripeSubscription.StripeSubscriptionId.Update(row);
        else db.StripeSubscription.Insert(row);
    }

    static void StripeUpsertCheckoutSession(Local db, Timestamp now, StripeCheckoutSession row)
    {
        var existing = db.StripeCheckoutSession.StripeCheckoutSessionId.Find(row.StripeCheckoutSessionId);
        if (existing is { } e)
        {
            row.StripeCustomerId ??= e.StripeCustomerId;
            row.MetadataJson ??= e.MetadataJson;
            row.InsertedAt = e.InsertedAt;
        }
        else row.InsertedAt = now;
        row.UpdatedAt = now;
        if (existing is not null) db.StripeCheckoutSession.StripeCheckoutSessionId.Update(row);
        else db.StripeCheckoutSession.Insert(row);
    }

    static void StripeUpsertPayment(Local db, Timestamp now, StripePayment row)
    {
        var existing = db.StripePayment.StripePaymentIntentId.Find(row.StripePaymentIntentId);
        if (existing is { } e)
        {
            row.StripeCustomerId ??= e.StripeCustomerId;
            row.StripeInvoiceId ??= e.StripeInvoiceId;
            row.MetadataJson ??= e.MetadataJson;
            row.OrgId ??= e.OrgId;
            row.UserId ??= e.UserId;
            row.InsertedAt = e.InsertedAt;
        }
        else row.InsertedAt = now;
        row.UpdatedAt = now;
        if (existing is not null) db.StripePayment.StripePaymentIntentId.Update(row);
        else db.StripePayment.Insert(row);
    }

    static void StripeUpsertInvoice(Local db, Timestamp now, StripeInvoice row)
    {
        var existing = db.StripeInvoice.StripeInvoiceId.Find(row.StripeInvoiceId);
        if (existing is { } e)
        {
            row.StripeSubscriptionId ??= e.StripeSubscriptionId;
            row.OrgId ??= e.OrgId;
            row.UserId ??= e.UserId;
            row.InsertedAt = e.InsertedAt;
        }
        else row.InsertedAt = now;
        row.UpdatedAt = now;
        if (existing is not null) db.StripeInvoice.StripeInvoiceId.Update(row);
        else db.StripeInvoice.Insert(row);
    }

    static void StripeUpdateWebhookStatus(Local db, Timestamp now, string eventId, StripeWebhookEventStatus status, string? error)
    {
        if (db.StripeWebhookEvent.EventId.Find(eventId) is not { } row) return;
        db.StripeWebhookEvent.EventId.Update(row with
        {
            Status = status,
            ErrorMessage = error,
            ProcessedAt = status != StripeWebhookEventStatus.Received ? now : row.ProcessedAt,
        });
    }

    // -----------------------------------------------------------------------
    // Out-of-order protection
    // -----------------------------------------------------------------------

    public static bool StripeIsStale((long Created, int Rank)? existing, (long Created, int Rank) next) =>
        existing is { } e && (next.Created < e.Created || next.Rank < e.Rank);

    public static int StripeSubscriptionStatusRank(string status) => status switch
    {
        "incomplete" => 0,
        "canceled" or "incomplete_expired" => 2,
        _ => 1,
    };

    public static int StripeInvoiceStatusRank(string status) => status switch
    {
        "draft" => 0,
        "uncollectible" => 2,
        "paid" or "void" => 3,
        _ => 1,
    };

    public static int StripeCheckoutPaymentStatusRank(string paymentStatus) => paymentStatus switch
    {
        "unpaid" => 0,
        "failed" => 1,
        _ => 2,
    };

    // -----------------------------------------------------------------------
    // Event validation (the TS valibot schemas) and application
    // -----------------------------------------------------------------------

    static readonly string[] StripeHandledEventTypes =
    [
        "customer.created", "customer.updated", "customer.subscription.created", "customer.subscription.updated",
        "customer.subscription.deleted", "checkout.session.completed", "checkout.session.async_payment_succeeded",
        "checkout.session.async_payment_failed", "invoice.created", "invoice.finalized", "invoice.paid",
        "invoice.payment_succeeded", "invoice.payment_failed", "invoice_payment.paid", "payment_intent.succeeded",
    ];

    /// <summary>A JS number. Big() is BigInt(n) then i64: non-integers and out-of-range values abort.</summary>
    readonly record struct StripeNum(double D, long? L)
    {
        public long Big()
        {
            if (L is { } l) return l;
            if (double.IsFinite(D) && Math.Floor(D) == D && D >= -9.223372036854775e18 && D < 9.223372036854775e18) return (long)D;
            throw new StripeAbortException($"RangeError: The number {D.ToString("R", CultureInfo.InvariantCulture)} cannot be converted to a BigInt or i64");
        }

        public static StripeNum? From(JsonNode? v)
        {
            if (v is not JsonValue x || x.GetValueKind() != JsonValueKind.Number) return null;
            return new StripeNum(x.GetValue<double>(), x.TryGetValue<long>(out var l) ? l : null);
        }
    }

    static long? StripeBig(StripeNum? n) => n?.Big();

    static StripeIssueException StripeMissing(string path) => new($"Invalid key: Expected property but received undefined at {path}");
    static StripeIssueException StripeWrongType(string expected, string path) => new($"Invalid type: Expected {expected} at {path}");

    static string StripeReqStr(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) ? throw StripeMissing(p + k) : StripeMaybeString(o[k]) ?? throw StripeWrongType("string", p + k);

    static string? StripeOptStr(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) ? null : StripeMaybeString(o[k]) ?? throw StripeWrongType("string", p + k);

    static string? StripeOptStrOrNull(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) || o[k] is null ? null : StripeMaybeString(o[k]) ?? throw StripeWrongType("string | null", p + k);

    static StripeNum StripeReqNum(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) ? throw StripeMissing(p + k) : StripeNum.From(o[k]) ?? throw StripeWrongType("number", p + k);

    static StripeNum? StripeOptNum(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) ? null : StripeNum.From(o[k]) ?? throw StripeWrongType("number", p + k);

    static StripeNum? StripeOptNumOrNull(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) || o[k] is null ? null : StripeNum.From(o[k]) ?? throw StripeWrongType("number | null", p + k);

    static bool? StripeOptBool(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) ? null : StripeMaybeBool(o[k]) ?? throw StripeWrongType("boolean", p + k);

    static JsonObject StripeReqObj(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) ? throw StripeMissing(p + k) : o[k] as JsonObject ?? throw StripeWrongType("Object", p + k);

    static string StripeExpandable(JsonNode? v, string path) => v switch
    {
        JsonValue x when x.GetValueKind() == JsonValueKind.String => x.GetValue<string>(),
        JsonObject m => StripeReqStr(m, "id", path + "."),
        _ => throw StripeWrongType("string | Object", path),
    };

    static string StripeReqExpandable(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) ? throw StripeMissing(p + k) : StripeExpandable(o[k], p + k);

    static string? StripeOptExpandable(JsonObject o, string k, string p) => !o.ContainsKey(k) ? null : StripeExpandable(o[k], p + k);

    /// <summary>Required expandable-or-null: (present, id).</summary>
    static string? StripeReqExpandableOrNull(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) ? throw StripeMissing(p + k) : o[k] is null ? null : StripeExpandable(o[k], p + k);

    /// <summary>Optional expandable-or-null: Present is false when the key is missing.</summary>
    static (bool Present, string? Id) StripeOptExpandableOrNull(JsonObject o, string k, string p) =>
        !o.ContainsKey(k) ? (false, null) : o[k] is null ? (true, null) : (true, StripeExpandable(o[k], p + k));

    static JsonObject? StripeMetadata(JsonObject o, string p)
    {
        if (!o.ContainsKey("metadata") || o["metadata"] is null) return null;
        if (o["metadata"] is not JsonObject m) throw StripeWrongType("Object | null", p + "metadata");
        foreach (var (k, v) in m)
        {
            if (StripeMaybeString(v) is null) throw StripeWrongType("string", $"{p}metadata.{k}");
        }
        return m;
    }

    static (string? Json, string? OrgId, string? UserId) StripeRecordInfo(JsonObject? m) =>
        m is null ? (null, null, null) : (StripeJsJson(m), StripeMaybeString(m["orgId"]), StripeMaybeString(m["userId"]));

    /// <summary>Apply a payload. Throws StripeAbortException where TS would throw (aborting the transaction).</summary>
    public static (StripeWebhookEventStatus Status, string? Error) StripeApplyEvent(Local db, Timestamp now, string payloadJson)
    {
        var parsed = StripeParse(payloadJson);
        if (parsed is null && payloadJson.Trim() != "null") return (StripeWebhookEventStatus.Failed, "invalid JSON payload");
        try
        {
            return (StripeDispatch(db, now, parsed), null);
        }
        catch (StripeIssueException issue)
        {
            var handled = StripeMaybeString(StripeGet(parsed, "type")) is { } t && StripeHandledEventTypes.Contains(t);
            return handled ? (StripeWebhookEventStatus.Failed, $"payload validation failed: {issue.Message}") : (StripeWebhookEventStatus.Ignored, null);
        }
    }

    static StripeWebhookEventStatus StripeDispatch(Local db, Timestamp now, JsonNode? v)
    {
        if (v is not JsonObject root) throw StripeWrongType("Object", "");
        var t = StripeMaybeString(root["type"]) is { } ty && StripeHandledEventTypes.Contains(ty)
            ? ty
            : throw new StripeIssueException("Invalid type: Expected a handled event type at type");
        var created = StripeReqNum(root, "created", "");
        var data = StripeReqObj(root, "data", "");
        var o = StripeReqObj(data, "object", "data.");
        const string p = "data.object.";
        switch (t)
        {
            case "customer.created":
            case "customer.updated":
            {
                var id = StripeReqStr(o, "id", p);
                var email = StripeOptStrOrNull(o, "email", p);
                var name = StripeOptStrOrNull(o, "name", p);
                var metadata = StripeMetadata(o, p);
                var eventCreated = created.Big();
                var existing = db.StripeCustomer.StripeCustomerId.Find(id);
                if (existing is { } e && eventCreated < e.EventCreatedUnix) return StripeWebhookEventStatus.Ignored;
                var (json, _, userId) = StripeRecordInfo(metadata);
                StripeUpsertCustomer(db, now, new StripeCustomer
                {
                    StripeCustomerId = id, AppUserId = null, Email = email, Name = name, MetadataJson = json, UserId = userId,
                    EventCreatedUnix = eventCreated, CreatedAt = now, UpdatedAt = now,
                });
                return StripeWebhookEventStatus.Processed;
            }
            case "customer.subscription.created":
            case "customer.subscription.updated":
            case "customer.subscription.deleted":
            {
                var id = StripeReqStr(o, "id", p);
                var customer = StripeReqExpandable(o, "customer", p);
                var rawStatus = StripeReqStr(o, "status", p);
                var currentPeriodEnd = StripeOptNum(o, "current_period_end", p);
                var cancelAt = StripeOptNumOrNull(o, "cancel_at", p);
                var cancelAtPeriodEndFlag = StripeOptBool(o, "cancel_at_period_end", p);
                StripeNum? firstEnd = null, firstQty = null;
                string? firstPrice = null;
                var hasFirst = false;
                if (o.ContainsKey("items"))
                {
                    if (o["items"] is not JsonObject items) throw StripeWrongType("Object", p + "items");
                    if (!items.ContainsKey("data")) throw StripeMissing(p + "items.data");
                    if (items["data"] is not JsonArray list) throw StripeWrongType("Array", p + "items.data");
                    for (var i = 0; i < list.Count; i++)
                    {
                        var ip = $"{p}items.data.{i}.";
                        if (list[i] is not JsonObject item) throw StripeWrongType("Object", ip.TrimEnd('.'));
                        string? priceId = null;
                        if (item.ContainsKey("price") && item["price"] is not null)
                        {
                            if (item["price"] is not JsonObject pr) throw StripeWrongType("Object | null", ip + "price");
                            priceId = StripeReqStr(pr, "id", ip + "price.");
                        }
                        var end = StripeOptNum(item, "current_period_end", ip);
                        var qty = StripeOptNum(item, "quantity", ip);
                        if (!hasFirst)
                        {
                            hasFirst = true;
                            (firstEnd, firstQty, firstPrice) = (end, qty, priceId);
                        }
                    }
                }
                var metadata = StripeMetadata(o, p);
                var eventCreated = created.Big();
                var status = t == "customer.subscription.deleted" ? "canceled" : rawStatus;
                var existing = db.StripeSubscription.StripeSubscriptionId.Find(id);
                if (StripeIsStale(existing is { } e ? (e.EventCreatedUnix, StripeSubscriptionStatusRank(e.Status)) : null,
                        (eventCreated, StripeSubscriptionStatusRank(status))))
                {
                    return StripeWebhookEventStatus.Ignored;
                }
                var periodEnd = StripeBig(firstEnd) ?? StripeBig(currentPeriodEnd) ?? 0;
                var cancelAtUnix = StripeBig(cancelAt);
                var (json, orgId, userId) = StripeRecordInfo(metadata);
                StripeUpsertSubscription(db, now, new StripeSubscription
                {
                    StripeSubscriptionId = id, StripeCustomerId = customer, Status = status, CurrentPeriodEndUnix = periodEnd,
                    CancelAtPeriodEnd = cancelAtPeriodEndFlag ?? StripeDeriveCancelAtPeriodEnd(cancelAtUnix, periodEnd),
                    CancelAtUnix = cancelAtUnix, Quantity = StripeBig(firstQty), PriceId = firstPrice, MetadataJson = json,
                    OrgId = orgId, UserId = userId, EventCreatedUnix = eventCreated, InsertedAt = now, UpdatedAt = now,
                });
                return StripeWebhookEventStatus.Processed;
            }
            case "checkout.session.completed":
            case "checkout.session.async_payment_succeeded":
            case "checkout.session.async_payment_failed":
            {
                var id = StripeReqStr(o, "id", p);
                var sessionStatus = StripeOptStrOrNull(o, "status", p);
                var rawPaymentStatus = StripeReqStr(o, "payment_status", p);
                var mode = StripeOptStr(o, "mode", p);
                var (_, customer) = StripeOptExpandableOrNull(o, "customer", p);
                var metadata = StripeMetadata(o, p);
                var eventCreated = created.Big();
                // Stripe reports a failed delayed payment as unpaid; record it as failed.
                var paymentStatus = t == "checkout.session.async_payment_failed" ? "failed" : rawPaymentStatus;
                var existing = db.StripeCheckoutSession.StripeCheckoutSessionId.Find(id);
                if (StripeIsStale(existing is { } e ? (e.EventCreatedUnix, StripeCheckoutPaymentStatusRank(e.PaymentStatus)) : null,
                        (eventCreated, StripeCheckoutPaymentStatusRank(paymentStatus))))
                {
                    return StripeWebhookEventStatus.Ignored;
                }
                StripeUpsertCheckoutSession(db, now, new StripeCheckoutSession
                {
                    StripeCheckoutSessionId = id, StripeCustomerId = customer, Status = sessionStatus ?? "complete",
                    PaymentStatus = paymentStatus, Mode = mode ?? "payment", MetadataJson = StripeRecordInfo(metadata).Json,
                    EventCreatedUnix = eventCreated, InsertedAt = now, UpdatedAt = now,
                });
                return StripeWebhookEventStatus.Processed;
            }
            case "invoice.created":
            case "invoice.finalized":
            case "invoice.paid":
            case "invoice.payment_succeeded":
            case "invoice.payment_failed":
            {
                var id = StripeReqStr(o, "id", p);
                var customer = StripeReqExpandableOrNull(o, "customer", p);
                var (_, legacy) = StripeOptExpandableOrNull(o, "subscription", p);
                string? parentSub = null;
                JsonObject? parentMeta = null;
                if (o.ContainsKey("parent") && o["parent"] is not null)
                {
                    if (o["parent"] is not JsonObject parent) throw StripeWrongType("Object | null", p + "parent");
                    if (parent.ContainsKey("subscription_details") && parent["subscription_details"] is not null)
                    {
                        if (parent["subscription_details"] is not JsonObject sd) throw StripeWrongType("Object | null", p + "parent.subscription_details");
                        const string sp = p + "parent.subscription_details.";
                        parentSub = StripeReqExpandable(sd, "subscription", sp);
                        parentMeta = StripeMetadata(sd, sp);
                    }
                }
                var status = StripeOptStrOrNull(o, "status", p) ?? (t switch
                {
                    "invoice.created" => "draft",
                    "invoice.finalized" or "invoice.payment_failed" => "open",
                    _ => "paid",
                });
                var amountDue = StripeOptNum(o, "amount_due", p);
                var amountPaid = StripeOptNum(o, "amount_paid", p);
                var invoiceCreated = StripeOptNum(o, "created", p);
                return StripeSyncInvoice(db, now, id, customer, legacy ?? parentSub, parentMeta, status, amountDue, amountPaid, invoiceCreated, created.Big());
            }
            case "invoice_payment.paid":
            {
                var invoice = StripeReqExpandable(o, "invoice", p);
                if (!o.ContainsKey("amount_paid")) throw StripeMissing(p + "amount_paid");
                StripeNum? amountPaid = o["amount_paid"] is null ? null : StripeNum.From(o["amount_paid"]) ?? throw StripeWrongType("number | null", p + "amount_paid");
                var currency = StripeReqStr(o, "currency", p);
                var paidCreated = StripeReqNum(o, "created", p);
                var payment = StripeReqObj(o, "payment", p);
                var paymentIntent = StripeOptExpandable(payment, "payment_intent", p + "payment.");
                created.Big();
                if (paymentIntent is null) return StripeWebhookEventStatus.Ignored;
                var existing = db.StripePayment.StripePaymentIntentId.Find(paymentIntent);
                var inv = db.StripeInvoice.StripeInvoiceId.Find(invoice);
                StripeUpsertPayment(db, now, new StripePayment
                {
                    StripePaymentIntentId = paymentIntent,
                    StripeCustomerId = inv?.StripeCustomerId,
                    StripeInvoiceId = invoice,
                    Amount = existing?.Amount ?? StripeBig(amountPaid) ?? 0,
                    Currency = existing?.Currency ?? currency,
                    Status = existing?.Status ?? "succeeded",
                    CreatedUnix = existing?.CreatedUnix ?? paidCreated.Big(),
                    MetadataJson = null,
                    OrgId = inv?.OrgId,
                    UserId = inv?.UserId,
                    InsertedAt = now,
                    UpdatedAt = now,
                });
                return StripeWebhookEventStatus.Processed;
            }
            default:
            {
                var id = StripeReqStr(o, "id", p);
                var (_, customer) = StripeOptExpandableOrNull(o, "customer", p);
                var (_, invoice) = StripeOptExpandableOrNull(o, "invoice", p);
                var amount = StripeOptNum(o, "amount", p);
                var currency = StripeOptStr(o, "currency", p);
                var status = StripeOptStr(o, "status", p);
                var piCreated = StripeOptNum(o, "created", p);
                var metadata = StripeMetadata(o, p);
                created.Big();
                var (json, orgId, userId) = StripeRecordInfo(metadata);
                StripeUpsertPayment(db, now, new StripePayment
                {
                    StripePaymentIntentId = id, StripeCustomerId = customer, StripeInvoiceId = invoice,
                    Amount = StripeBig(amount) ?? 0, Currency = currency ?? "unknown", Status = status ?? "succeeded",
                    CreatedUnix = StripeBig(piCreated) ?? 0, MetadataJson = json, OrgId = orgId, UserId = userId,
                    InsertedAt = now, UpdatedAt = now,
                });
                return StripeWebhookEventStatus.Processed;
            }
        }
    }

    static StripeWebhookEventStatus StripeSyncInvoice(Local db, Timestamp now, string id, string? payloadCustomer, string? payloadSubscription,
        JsonObject? parentMeta, string status, StripeNum? amountDue, StripeNum? amountPaid, StripeNum? invoiceCreated, long eventCreated)
    {
        var existing = db.StripeInvoice.StripeInvoiceId.Find(id);
        if (StripeIsStale(existing is { } e0 ? (e0.EventCreatedUnix, StripeInvoiceStatusRank(e0.Status)) : null,
                (eventCreated, StripeInvoiceStatusRank(status))))
        {
            return StripeWebhookEventStatus.Ignored;
        }
        if ((existing?.StripeCustomerId ?? payloadCustomer) is not { } customerId) return StripeWebhookEventStatus.Failed;
        var subscriptionId = existing?.StripeSubscriptionId ?? payloadSubscription;
        var subscription = string.IsNullOrEmpty(subscriptionId) ? null : db.StripeSubscription.StripeSubscriptionId.Find(subscriptionId);
        var orgId = existing?.OrgId ?? subscription?.OrgId ?? StripeMaybeString(parentMeta?["orgId"]);
        var userId = existing?.UserId ?? subscription?.UserId ?? StripeMaybeString(parentMeta?["userId"]);
        // A payment can arrive before its invoice; fill what it couldn't resolve.
        foreach (var payment in db.StripePayment.Iter().Where(pm => pm.StripeInvoiceId == id).ToList())
        {
            var filled = payment with
            {
                StripeCustomerId = payment.StripeCustomerId ?? customerId,
                OrgId = payment.OrgId ?? orgId,
                UserId = payment.UserId ?? userId,
            };
            if (filled.StripeCustomerId != payment.StripeCustomerId || filled.OrgId != payment.OrgId || filled.UserId != payment.UserId)
            {
                db.StripePayment.StripePaymentIntentId.Update(filled);
            }
        }
        StripeUpsertInvoice(db, now, new StripeInvoice
        {
            StripeInvoiceId = id,
            StripeCustomerId = customerId,
            StripeSubscriptionId = subscriptionId,
            Status = status,
            AmountDue = StripeBig(amountDue) ?? existing?.AmountDue ?? 0,
            AmountPaid = StripeBig(amountPaid) ?? existing?.AmountPaid ?? 0,
            CreatedUnix = StripeBig(invoiceCreated) ?? existing?.CreatedUnix ?? 0,
            OrgId = orgId,
            UserId = userId,
            EventCreatedUnix = eventCreated,
            InsertedAt = now,
            UpdatedAt = now,
        });
        return StripeWebhookEventStatus.Processed;
    }

    /// <summary>id, type and livemode from a signed payload.</summary>
    public static (string Id, string Type, bool Livemode)? StripeParseEventMetadata(string payloadJson)
    {
        if (StripeParse(payloadJson) is not JsonObject o) return null;
        if (StripeMaybeString(o["id"]) is not { } id || StripeMaybeString(o["type"]) is not { } type) return null;
        return (id, type, StripeMaybeBool(o["livemode"]) ?? false);
    }

    /// <summary>Store and apply a verified event. Applied is false for a processed/ignored duplicate.</summary>
    static (bool Applied, StripeWebhookEventStatus Status, string? Error) StripeStoreAndApply(Local db, Timestamp now, string eventId, string eventType, bool livemode, string payloadJson)
    {
        var existing = db.StripeWebhookEvent.EventId.Find(eventId);
        if (existing is { Status: StripeWebhookEventStatus.Processed or StripeWebhookEventStatus.Ignored } done) return (false, done.Status, null);
        if (existing is null)
        {
            db.StripeWebhookEvent.Insert(new StripeWebhookEvent
            {
                EventId = eventId, EventType = eventType, Livemode = livemode, PayloadJson = payloadJson,
                Status = StripeWebhookEventStatus.Received, ErrorMessage = null, ReceivedAt = now, ProcessedAt = null,
            });
        }
        var (status, error) = StripeApplyEvent(db, now, existing?.PayloadJson ?? payloadJson);
        StripeUpdateWebhookStatus(db, now, eventId, status, error);
        return (true, status, error);
    }

    /// <summary>Relay ingest: verifies the signature and the supplied metadata, then stores and applies the event.</summary>
    [SpacetimeDB.Reducer]
    public static void IngestStripeWebhook(ReducerContext ctx, string eventId, string eventType, bool livemode, string payloadJson, string? signatureHeader)
    {
        if (eventId.Length == 0 || eventId.Length > StripeMaxWebhookMetadataLength || eventType.Length == 0 || eventType.Length > StripeMaxWebhookMetadataLength)
        {
            throw new Exception(StripeErrors.WebhookMetadataInvalid);
        }
        if (payloadJson.Length > StripeMaxWebhookBodyLength) throw new Exception(StripeErrors.WebhookPayloadTooLarge);
        if ((signatureHeader?.Length ?? 0) > StripeMaxWebhookHeaderLength) throw new Exception(StripeErrors.WebhookSignatureTooLarge);
        if (ctx.Db.StripeConfig.Singleton.Find(true)?.WebhookSigningSecret is not { Length: > 0 } secret)
        {
            throw new Exception(StripeErrors.WebhookSecretNotConfigured);
        }
        var verified = Crypto.VerifyStripeSignature(new()
        {
            RawBody = payloadJson,
            SignatureHeader = signatureHeader ?? "",
            Secret = secret,
            ToleranceSeconds = Crypto.DefaultToleranceSeconds,
            NowSeconds = StripeUnixSeconds(ctx.Timestamp),
        });
        if (!verified.Ok) throw new Exception($"{StripeErrors.WebhookSignatureMismatch}:{verified.Reason}");
        var signed = StripeParseEventMetadata(payloadJson) ?? throw new Exception(StripeErrors.WebhookPayloadMissingMetadata);
        if (eventId != signed.Id || eventType != signed.Type || livemode != signed.Livemode) throw new Exception(StripeErrors.WebhookMetadataMismatch);
        StripeStoreAndApply(ctx.Db, ctx.Timestamp, signed.Id, signed.Type, signed.Livemode, payloadJson);
    }

    /// <summary>Admins only: re-apply a stored event (the same staleness rules apply).</summary>
    [SpacetimeDB.Reducer]
    public static void StripeReplayWebhookEvent(ReducerContext ctx, string eventId)
    {
        if (!StripeIsAdmin(ctx.Db, ctx.Sender)) throw new Exception(StripeErrors.NotAuthorized);
        var ev = ctx.Db.StripeWebhookEvent.EventId.Find(eventId) ?? throw new Exception(StripeErrors.WebhookEventNotFound);
        var (status, error) = StripeApplyEvent(ctx.Db, ctx.Timestamp, ev.PayloadJson);
        StripeUpdateWebhookStatus(ctx.Db, ctx.Timestamp, eventId, status, error);
    }

    /// <summary>Scheduled hourly by StripeInstall: deletes up to 500 events received over 30 days ago.</summary>
    [SpacetimeDB.Reducer]
    public static void StripePruneWebhookEvents(ReducerContext ctx, StripeWebhookPruneTick tick)
    {
        if (ctx.Sender != ctx.DatabaseIdentity) throw new Exception(StripeErrors.NotAuthorized);
        StripePruneWebhookEventsAt(ctx.Db, ctx.Timestamp);
    }

    /// <summary>The retention sweep, callable from your own code.</summary>
    public static int StripePruneWebhookEventsAt(Local db, Timestamp now)
    {
        var cutoff = new Timestamp(now.MicrosecondsSinceUnixEpoch - StripeWebhookEventRetentionMicros);
        var expired = db.StripeWebhookEvent.ReceivedAt.Filter(new Bound<Timestamp>(new Timestamp(long.MinValue), cutoff))
            .Where(e => e.ReceivedAt.MicrosecondsSinceUnixEpoch < cutoff.MicrosecondsSinceUnixEpoch)
            .Take(StripeWebhookPruneBatch)
            .Select(e => e.EventId)
            .ToList();
        foreach (var id in expired) db.StripeWebhookEvent.EventId.Delete(id);
        return expired.Count;
    }

    static HttpResponse StripeJsonResponse(ushort status, string body) =>
        new(status, HttpVersion.Http11, new List<HttpHeader> { new("content-type", "application/json") }, HttpBody.FromString(body));

    static HttpResponse StripeErrorJson(ushort status, string error) => StripeJsonResponse(status, "{\"error\":" + StripeJsString(error) + "}");

    static string? StripeHeader(HttpRequest req, string name)
    {
        var values = req.Headers
            .Where(h => string.Equals(h.Name, name, StringComparison.OrdinalIgnoreCase))
            .Select(h => Encoding.UTF8.GetString(h.Value))
            .ToList();
        return values.Count == 0 ? null : string.Join(", ", values);
    }

    /// <summary>HTTP route for Stripe's webhooks: register it on your router.</summary>
    [SpacetimeDB.HttpHandler]
    public static HttpResponse StripeWebhook(HandlerContext ctx, HttpRequest req)
    {
        var signature = StripeHeader(req, "stripe-signature");
        if (!string.Equals(req.Method.Value, "POST", StringComparison.Ordinal)) return StripeErrorJson(405, "method not allowed");
        var contentLength = double.TryParse(StripeHeader(req, "content-length")?.Trim(), NumberStyles.Float, CultureInfo.InvariantCulture, out var cl) ? cl : 0;
        if (contentLength > StripeMaxWebhookBodyLength) return StripeErrorJson(413, "payload too large");
        if ((signature?.Length ?? 0) > StripeMaxWebhookHeaderLength) return StripeErrorJson(431, "signature header too large");
        var payload = req.Body.ToStringUtf8Lossy();
        if (payload.Length == 0) return StripeErrorJson(400, "empty body");
        if (Encoding.UTF8.GetByteCount(payload) > StripeMaxWebhookBodyLength) return StripeErrorJson(413, "payload too large");
        var secret = ctx.WithTx(tx => tx.Db.StripeConfig.Singleton.Find(true)?.WebhookSigningSecret);
        if (string.IsNullOrEmpty(secret)) return StripeErrorJson(503, "webhook signing secret not configured");
        var verified = Crypto.VerifyStripeSignature(new()
        {
            RawBody = payload,
            SignatureHeader = signature ?? "",
            Secret = secret,
            ToleranceSeconds = Crypto.DefaultToleranceSeconds,
            NowSeconds = StripeUnixSeconds(ctx.Timestamp),
        });
        if (!verified.Ok) return StripeErrorJson(401, verified.Reason ?? "");
        if (StripeParseEventMetadata(payload) is not { } meta) return StripeErrorJson(400, "missing or invalid event metadata");
        if (meta.Id.Length == 0 || meta.Id.Length > StripeMaxWebhookMetadataLength || meta.Type.Length == 0 || meta.Type.Length > StripeMaxWebhookMetadataLength)
        {
            return StripeErrorJson(400, "invalid event metadata");
        }
        (bool Applied, StripeWebhookEventStatus Status, string? Error) outcome;
        try
        {
            outcome = ctx.WithTx(tx => StripeStoreAndApply(tx.Db, tx.Timestamp, meta.Id, meta.Type, meta.Livemode, payload));
        }
        catch (Exception e)
        {
            Log.Error($"stripe webhook processing failed for {meta.Id}: {e.Message}");
            return StripeErrorJson(500, "webhook processing failed");
        }
        var ok = outcome.Status != StripeWebhookEventStatus.Failed;
        var body = new StringBuilder("{\"ok\":").Append(ok ? "true" : "false")
            .Append(",\"eventId\":").Append(StripeJsString(meta.Id))
            .Append(",\"kind\":").Append(outcome.Applied ? "\"applied\"" : "\"duplicate\"")
            .Append(",\"status\":{\"tag\":\"").Append(outcome.Status.ToString()).Append("\"}");
        if (outcome.Error is not null) body.Append(",\"error\":").Append(StripeJsString(outcome.Error));
        return StripeJsonResponse(ok ? (ushort)200 : (ushort)400, body.Append('}').ToString());
    }

    // -----------------------------------------------------------------------
    // Billing
    // -----------------------------------------------------------------------

    static string StripeCreateCustomerInStripeAndSync(ProcedureContext ctx, StripeConfig cfg, string? email, string? name, string? metadataJson, string? idempotencyKey)
    {
        var pairs = new List<(string, string?)> { ("email", email), ("name", name) };
        pairs.AddRange(StripeMetadataJsonToFormPairs("metadata", metadataJson));
        var key = string.IsNullOrEmpty(idempotencyKey) ? null : $"create_customer_{idempotencyKey}";
        var result = StripeCall(ctx, cfg, "POST", "/v1/customers", StripeFormPairsToBody(pairs), key);
        StripeEnsureOk(result, StripeErrors.CreateCustomerFailed);
        if (StripeParse(result.Body) is not JsonObject o) throw new Exception($"{StripeErrors.CreateCustomerInvalidResponse}:Invalid type: Expected Object at ");
        string customerId;
        try { customerId = StripeReqStr(o, "id", ""); }
        catch (StripeIssueException issue) { throw new Exception($"{StripeErrors.CreateCustomerInvalidResponse}:{issue.Message}"); }
        var (json, _, userId) = StripeCoerceMetadataFromJson(metadataJson);
        var now = ctx.Timestamp;
        ctx.WithTx(tx =>
        {
            StripeUpsertCustomer(tx.Db, now, new StripeCustomer
            {
                StripeCustomerId = customerId, AppUserId = null, Email = email, Name = name, MetadataJson = json, UserId = userId,
                EventCreatedUnix = StripeUnixSeconds(now), CreatedAt = now, UpdatedAt = now,
            });
            return 0;
        });
        return customerId;
    }

    static string? StripeFindUserCustomerId(ProcedureContext ctx, string userId) => ctx.WithTx(tx =>
    {
        foreach (var c in tx.Db.StripeCustomer.Iter()) if (c.UserId == userId) return c.StripeCustomerId;
        foreach (var s in tx.Db.StripeSubscription.Iter()) if (s.UserId == userId) return s.StripeCustomerId;
        foreach (var pm in tx.Db.StripePayment.Iter())
        {
            if (pm.UserId == userId && !string.IsNullOrEmpty(pm.StripeCustomerId)) return pm.StripeCustomerId;
        }
        return null;
    });

    /// <summary>
    /// The Stripe customer for an application user, created with metadata.userId when none exists.
    /// Matches by userId only. No authorization: resolve userId from trusted context. Throws stripe.* codes.
    /// </summary>
    public static StripeGetOrCreateCustomerResult StripeGetOrCreateUserCustomer(ProcedureContext ctx, string userId, string? email = null, string? name = null)
    {
        if (StripeFindUserCustomerId(ctx, userId) is { Length: > 0 } existing) return new StripeGetOrCreateCustomerResult { CustomerId = existing, IsNew = false };
        var cfg = StripeConfigFor(ctx);
        var customerId = StripeCreateCustomerInStripeAndSync(ctx, cfg, email, name, "{\"userId\":" + StripeJsString(userId) + "}", userId);
        return new StripeGetOrCreateCustomerResult { CustomerId = customerId, IsNew = true };
    }

    /// <summary>Create a Checkout session. All items must share the session's mode. Throws stripe.* codes.</summary>
    public static StripeCheckoutSessionResult StripeCreateCheckoutSessionInStripe(ProcedureContext ctx, StripeCheckoutSessionArgs args)
    {
        var cfg = StripeConfigFor(ctx);
        if (args.Items.Count == 0) throw new Exception(StripeErrors.CheckoutSessionRequiresItems);
        var pairs = new List<(string, string?)>
        {
            ("mode", args.Mode), ("success_url", args.SuccessUrl), ("cancel_url", args.CancelUrl), ("customer", args.CustomerId),
        };
        for (var i = 0; i < args.Items.Count; i++)
        {
            pairs.Add(($"line_items[{i}][price]", args.Items[i].PriceId));
            pairs.Add(($"line_items[{i}][quantity]", args.Items[i].Quantity.ToString(CultureInfo.InvariantCulture)));
        }
        pairs.AddRange(StripeMetadataJsonToFormPairs("metadata", args.MetadataJson));
        if (args.Mode == "subscription") pairs.AddRange(StripeMetadataJsonToFormPairs("subscription_data[metadata]", args.SubscriptionMetadataJson));
        if (args.Mode == "payment") pairs.AddRange(StripeMetadataJsonToFormPairs("payment_intent_data[metadata]", args.PaymentIntentMetadataJson));
        var response = StripeCall(ctx, cfg, "POST", "/v1/checkout/sessions", StripeFormPairsToBody(pairs), null);
        StripeEnsureOk(response, StripeErrors.CheckoutSessionFailed);
        try
        {
            if (StripeParse(response.Body) is not JsonObject o) throw StripeWrongType("Object", "");
            return new StripeCheckoutSessionResult { SessionId = StripeReqStr(o, "id", ""), Url = StripeOptStrOrNull(o, "url", "") };
        }
        catch (StripeIssueException issue)
        {
            throw new Exception($"{StripeErrors.CheckoutSessionInvalidResponse}:{issue.Message}");
        }
    }

    /// <summary>
    /// A Checkout session for an application user's own customer: userId goes into the session,
    /// subscription and PaymentIntent metadata. No authorization. Throws stripe.* codes.
    /// </summary>
    public static StripeCheckoutSessionResult StripeCreateUserCheckoutSession(ProcedureContext ctx, StripeUserCheckoutArgs args)
    {
        var customer = StripeGetOrCreateUserCustomer(ctx, args.UserId, args.Email, args.Name);
        var meta = new JsonObject();
        foreach (var (k, v) in args.Metadata) meta[k] = JsonValue.Create(v);
        meta["userId"] = JsonValue.Create(args.UserId);
        var metadataJson = StripeJsJson(meta);
        return StripeCreateCheckoutSessionInStripe(ctx, new StripeCheckoutSessionArgs
        {
            Items = args.Items,
            CustomerId = customer.CustomerId,
            Mode = args.Mode,
            SuccessUrl = args.SuccessUrl,
            CancelUrl = args.CancelUrl,
            MetadataJson = metadataJson,
            SubscriptionMetadataJson = metadataJson,
            PaymentIntentMetadataJson = metadataJson,
        });
    }

    /// <summary>Admins only: confirms a price exists and is active.</summary>
    [SpacetimeDB.Procedure]
    public static Result<ValidateStripePriceResult, string> ValidateStripePrice(ProcedureContext ctx, string priceId) => StripeAdmin(ctx, () =>
    {
        var cfg = StripeConfigFor(ctx);
        var response = StripeCall(ctx, cfg, "GET", $"/v1/prices/{priceId}", null, null);
        var parsed = StripeParse(response.Body);
        if (response.Status is < 200 or >= 300)
        {
            var err = StripeIsRecord(parsed) && StripeGet(parsed, "error") is { } e && StripeIsRecord(e) ? e : null;
            return new ValidateStripePriceResult
            {
                Valid = false, Status = response.Status,
                Message = StripeMaybeString(StripeGet(err, "message")) ?? $"Stripe returned {response.Status}.",
                Code = StripeMaybeString(StripeGet(err, "code")), ErrorType = StripeMaybeString(StripeGet(err, "type")),
            };
        }
        var data = StripeIsRecord(parsed) ? parsed : null;
        return new ValidateStripePriceResult
        {
            Valid = true, Status = response.Status, Active = StripeMaybeBool(StripeGet(data, "active")),
            Currency = StripeMaybeString(StripeGet(data, "currency")), UnitAmount = StripeMaybeI64(StripeGet(data, "unit_amount")),
            Livemode = StripeMaybeBool(StripeGet(data, "livemode")), PriceType = StripeMaybeString(StripeGet(data, "type")),
        };
    });

    /// <summary>Admins only: fetch a Checkout session's state from Stripe.</summary>
    [SpacetimeDB.Procedure]
    public static Result<StripeRemoteCheckoutSessionResult, string> StripeGetRemoteCheckoutSession(ProcedureContext ctx, string sessionId) => StripeAdmin(ctx, () =>
    {
        var cfg = StripeConfigFor(ctx);
        var response = StripeCall(ctx, cfg, "GET", $"/v1/checkout/sessions/{sessionId}", null, null);
        var parsed = StripeParse(response.Body);
        if (response.Status is < 200 or >= 300)
        {
            var err = StripeIsRecord(parsed) && StripeGet(parsed, "error") is { } e && StripeIsRecord(e) ? e : null;
            return new StripeRemoteCheckoutSessionResult
            {
                Ok = false, Status = response.Status,
                Message = StripeMaybeString(StripeGet(err, "message")) ?? $"Stripe returned {response.Status}.",
                Code = StripeMaybeString(StripeGet(err, "code")), ErrorType = StripeMaybeString(StripeGet(err, "type")),
            };
        }
        var data = StripeIsRecord(parsed) ? parsed : null;
        return new StripeRemoteCheckoutSessionResult
        {
            Ok = true, Status = response.Status, SessionId = StripeMaybeString(StripeGet(data, "id")) ?? sessionId,
            PaymentStatus = StripeMaybeString(StripeGet(data, "payment_status")), SessionStatus = StripeMaybeString(StripeGet(data, "status")),
            Mode = StripeMaybeString(StripeGet(data, "mode")), AmountTotal = StripeMaybeI64(StripeGet(data, "amount_total")),
            Currency = StripeMaybeString(StripeGet(data, "currency")), CustomerId = StripeMaybeId(StripeGet(data, "customer")),
            PaymentIntentId = StripeMaybeId(StripeGet(data, "payment_intent")),
        };
    });

    /// <summary>Admins only: how many webhook events are stored.</summary>
    [SpacetimeDB.Procedure]
    public static Result<long, string> StripeGetWebhookEventCount(ProcedureContext ctx) =>
        StripeAdmin(ctx, () => ctx.WithTx(tx => (long)tx.Db.StripeWebhookEvent.Count));

    /// <summary>Admins only: replace a subscription's local metadata (and orgId / userId).</summary>
    [SpacetimeDB.Procedure]
    public static Result<Unit, string> StripeUpdateSubscriptionMetadata(ProcedureContext ctx, string stripeSubscriptionId, string metadataJson, string? orgId, string? userId) => StripeAdmin(ctx, () =>
    {
        var (json, parsedOrg, parsedUser) = StripeCoerceMetadataFromJson(metadataJson);
        var now = ctx.Timestamp;
        var error = ctx.WithTx(tx =>
        {
            if (tx.Db.StripeSubscription.StripeSubscriptionId.Find(stripeSubscriptionId) is not { } existing) return StripeErrors.SubscriptionNotFound;
            StripeUpsertSubscription(tx.Db, now, existing with
            {
                MetadataJson = json ?? metadataJson,
                OrgId = orgId ?? parsedOrg ?? existing.OrgId,
                UserId = userId ?? parsedUser ?? existing.UserId,
            });
            return (string?)null;
        });
        if (error is not null) throw new Exception(error);
        return default(Unit);
    });

    /// <summary>Admins only.</summary>
    [SpacetimeDB.Procedure]
    public static Result<StripeGetOrCreateCustomerResult, string> StripeGetOrCreateCustomer(ProcedureContext ctx, string userId, string? email, string? name) =>
        StripeAdmin(ctx, () => StripeGetOrCreateUserCustomer(ctx, userId, email, name));

    /// <summary>Admins only.</summary>
    [SpacetimeDB.Procedure]
    public static Result<StripeCheckoutSessionResult, string> StripeCreateCheckoutSession(ProcedureContext ctx, List<StripeCheckoutLineItem> items, string? customerId,
        string mode, string successUrl, string cancelUrl, string? metadataJson, string? subscriptionMetadataJson, string? paymentIntentMetadataJson) =>
        StripeAdmin(ctx, () => StripeCreateCheckoutSessionInStripe(ctx, new StripeCheckoutSessionArgs
        {
            Items = items, CustomerId = customerId, Mode = mode, SuccessUrl = successUrl, CancelUrl = cancelUrl, MetadataJson = metadataJson,
            SubscriptionMetadataJson = subscriptionMetadataJson, PaymentIntentMetadataJson = paymentIntentMetadataJson,
        }));

    /// <summary>Admins only.</summary>
    [SpacetimeDB.Procedure]
    public static Result<StripePortalSessionResult, string> StripeCreateCustomerPortalSession(ProcedureContext ctx, string customerId, string returnUrl) => StripeAdmin(ctx, () =>
    {
        var cfg = StripeConfigFor(ctx);
        var body = StripeFormPairsToBody([("customer", customerId), ("return_url", returnUrl)]);
        var response = StripeCall(ctx, cfg, "POST", "/v1/billing_portal/sessions", body, null);
        StripeEnsureOk(response, StripeErrors.PortalSessionFailed);
        try
        {
            if (StripeParse(response.Body) is not JsonObject o) throw StripeWrongType("Object", "");
            return new StripePortalSessionResult { Url = StripeReqStr(o, "url", "") };
        }
        catch (StripeIssueException issue)
        {
            throw new Exception($"{StripeErrors.PortalSessionInvalidResponse}:{issue.Message}");
        }
    });

    static JsonNode StripePatchSubscription(ProcedureContext ctx, StripeConfig cfg, string subscriptionId, string formBody)
    {
        var response = StripeCall(ctx, cfg, "POST", $"/v1/subscriptions/{subscriptionId}", formBody, null);
        StripeEnsureOk(response, StripeErrors.SubscriptionUpdateFailed);
        return StripeParse(response.Body) is { } v && StripeIsRecord(v) ? v : throw new Exception(StripeErrors.SubscriptionUpdateInvalidResponse);
    }

    static void StripeSyncSubscriptionFromStripe(ProcedureContext ctx, JsonNode sub)
    {
        var subscriptionId = StripeMaybeString(StripeGet(sub, "id"));
        var customerId = StripeMaybeId(StripeGet(sub, "customer"));
        var status = StripeMaybeString(StripeGet(sub, "status"));
        if (string.IsNullOrEmpty(subscriptionId) || string.IsNullOrEmpty(customerId) || string.IsNullOrEmpty(status))
        {
            throw new Exception(StripeErrors.SubscriptionPayloadMissingFields);
        }
        var items = StripeGet(sub, "items") is { } it && StripeIsRecord(it) ? it : null;
        var first = StripeGet(items, "data") is JsonArray { Count: > 0 } arr && StripeIsRecord(arr[0]) ? arr[0] : null;
        var price = StripeGet(first, "price") is { } pr && StripeIsRecord(pr) ? pr : null;
        var periodEnd = StripeMaybeI64(StripeGet(first, "current_period_end")) ?? StripeMaybeI64(StripeGet(sub, "current_period_end")) ?? 0;
        var cancelAtUnix = StripeMaybeI64(StripeGet(sub, "cancel_at"));
        var cancelAtPeriodEnd = StripeMaybeBool(StripeGet(sub, "cancel_at_period_end")) ?? StripeDeriveCancelAtPeriodEnd(cancelAtUnix, periodEnd);
        var quantity = StripeMaybeI64(StripeGet(first, "quantity"));
        var (json, orgId, userId) = StripeMetadataInfo(StripeGet(sub, "metadata"));
        var priceId = StripeMaybeString(StripeGet(price, "id"));
        var now = ctx.Timestamp;
        ctx.WithTx(tx =>
        {
            StripeUpsertSubscription(tx.Db, now, new StripeSubscription
            {
                StripeSubscriptionId = subscriptionId, StripeCustomerId = customerId, Status = status, CurrentPeriodEndUnix = periodEnd,
                CancelAtPeriodEnd = cancelAtPeriodEnd, CancelAtUnix = cancelAtUnix, Quantity = quantity, PriceId = priceId,
                MetadataJson = json, OrgId = orgId, UserId = userId, EventCreatedUnix = StripeUnixSeconds(now), InsertedAt = now, UpdatedAt = now,
            });
            return 0;
        });
    }

    /// <summary>Admins only. Cancels at period end unless cancelAtPeriodEnd is false.</summary>
    [SpacetimeDB.Procedure]
    public static Result<Unit, string> StripeCancelSubscription(ProcedureContext ctx, string stripeSubscriptionId, bool? cancelAtPeriodEnd) => StripeAdmin(ctx, () =>
    {
        var cfg = StripeConfigFor(ctx);
        JsonNode sub;
        if (cancelAtPeriodEnd ?? true)
        {
            sub = StripePatchSubscription(ctx, cfg, stripeSubscriptionId, "cancel_at_period_end=true");
        }
        else
        {
            var response = StripeCall(ctx, cfg, "DELETE", $"/v1/subscriptions/{stripeSubscriptionId}", null, null);
            StripeEnsureOk(response, StripeErrors.SubscriptionCancelFailed);
            sub = StripeParse(response.Body) is { } v && StripeIsRecord(v) ? v : throw new Exception(StripeErrors.SubscriptionCancelInvalidResponse);
        }
        StripeSyncSubscriptionFromStripe(ctx, sub);
        return default(Unit);
    });

    /// <summary>Admins only.</summary>
    [SpacetimeDB.Procedure]
    public static Result<Unit, string> StripeReactivateSubscription(ProcedureContext ctx, string stripeSubscriptionId) => StripeAdmin(ctx, () =>
    {
        var cfg = StripeConfigFor(ctx);
        StripeSyncSubscriptionFromStripe(ctx, StripePatchSubscription(ctx, cfg, stripeSubscriptionId, "cancel_at_period_end=false"));
        return default(Unit);
    });

    /// <summary>Admins only: sets the first line item's quantity.</summary>
    [SpacetimeDB.Procedure]
    public static Result<Unit, string> StripeUpdateSubscriptionQuantity(ProcedureContext ctx, string stripeSubscriptionId, long quantity) => StripeAdmin(ctx, () =>
    {
        var cfg = StripeConfigFor(ctx);
        var get = StripeCall(ctx, cfg, "GET", $"/v1/subscriptions/{stripeSubscriptionId}", null, null);
        StripeEnsureOk(get, StripeErrors.SubscriptionLookupFailed);
        var existing = StripeParse(get.Body) is { } v && StripeIsRecord(v) ? v : throw new Exception(StripeErrors.SubscriptionLookupInvalidResponse);
        var items = StripeGet(existing, "items") is { } it && StripeIsRecord(it) ? it : null;
        var first = StripeGet(items, "data") is JsonArray { Count: > 0 } arr && StripeIsRecord(arr[0]) ? arr[0] : null;
        if (StripeMaybeString(StripeGet(first, "id")) is not { Length: > 0 } itemId) throw new Exception(StripeErrors.SubscriptionMissingLineItems);
        var update = StripeCall(ctx, cfg, "POST", $"/v1/subscription_items/{itemId}", $"quantity={quantity.ToString(CultureInfo.InvariantCulture)}", null);
        StripeEnsureOk(update, StripeErrors.SubscriptionItemUpdateFailed);
        var now = ctx.Timestamp;
        ctx.WithTx(tx =>
        {
            if (tx.Db.StripeSubscription.StripeSubscriptionId.Find(stripeSubscriptionId) is { } local)
            {
                StripeUpsertSubscription(tx.Db, now, local with { Quantity = quantity });
            }
            return 0;
        });
        return default(Unit);
    });

    // -----------------------------------------------------------------------
    // Admin queries (at most 1,000 rows each)
    // -----------------------------------------------------------------------

    static Result<T, string> StripeAdminRead<T>(ProcedureContext ctx, Func<Local, T> read) => StripeAdmin(ctx, () => ctx.WithTx(tx => read(tx.Db)));

    static List<StripeSubscription> StripeSubscriptionsWhere(Local db, Func<StripeSubscription, bool> f) =>
        db.StripeSubscription.Iter().Where(f).OrderBy(s => s.InsertedAt.MicrosecondsSinceUnixEpoch).ToList();

    [SpacetimeDB.Procedure]
    public static Result<StripeCustomer?, string> StripeGetCustomer(ProcedureContext ctx, string stripeCustomerId) =>
        StripeAdminRead(ctx, db => db.StripeCustomer.StripeCustomerId.Find(stripeCustomerId));

    [SpacetimeDB.Procedure]
    public static Result<StripeCustomer?, string> StripeGetCustomerByUserId(ProcedureContext ctx, string userId) =>
        StripeAdminRead(ctx, db =>
        {
            foreach (var c in db.StripeCustomer.Iter()) if (c.UserId == userId) return (StripeCustomer?)c;
            return null;
        });

    [SpacetimeDB.Procedure]
    public static Result<StripeSubscription?, string> StripeGetSubscription(ProcedureContext ctx, string stripeSubscriptionId) =>
        StripeAdminRead(ctx, db => db.StripeSubscription.StripeSubscriptionId.Find(stripeSubscriptionId));

    [SpacetimeDB.Procedure]
    public static Result<List<StripeSubscription>, string> StripeListSubscriptions(ProcedureContext ctx, string stripeCustomerId) =>
        StripeAdminRead(ctx, db => db.StripeSubscription.StripeCustomerId.Filter(stripeCustomerId).Take(StripeMaxQueryRows).ToList());

    [SpacetimeDB.Procedure]
    public static Result<StripeSubscription?, string> StripeGetSubscriptionByOrgId(ProcedureContext ctx, string orgId) =>
        StripeAdminRead(ctx, db =>
        {
            var rows = StripeSubscriptionsWhere(db, s => s.OrgId == orgId);
            if (rows.Count == 0) return (StripeSubscription?)null;
            return rows.OrderBy(s => s.InsertedAt.MicrosecondsSinceUnixEpoch).ThenBy(s => s.StripeSubscriptionId, StringComparer.Ordinal).Last();
        });

    [SpacetimeDB.Procedure]
    public static Result<List<StripeSubscription>, string> StripeListSubscriptionsByOrgId(ProcedureContext ctx, string orgId) =>
        StripeAdminRead(ctx, db => StripeSubscriptionsWhere(db, s => s.OrgId == orgId).Take(StripeMaxQueryRows).ToList());

    [SpacetimeDB.Procedure]
    public static Result<List<StripeSubscription>, string> StripeListSubscriptionsByUserId(ProcedureContext ctx, string userId) =>
        StripeAdminRead(ctx, db => StripeSubscriptionsWhere(db, s => s.UserId == userId).Take(StripeMaxQueryRows).ToList());

    [SpacetimeDB.Procedure]
    public static Result<StripePayment?, string> StripeGetPayment(ProcedureContext ctx, string stripePaymentIntentId) =>
        StripeAdminRead(ctx, db => db.StripePayment.StripePaymentIntentId.Find(stripePaymentIntentId));

    [SpacetimeDB.Procedure]
    public static Result<List<StripePayment>, string> StripeListPayments(ProcedureContext ctx, string stripeCustomerId) =>
        StripeAdminRead(ctx, db => db.StripePayment.Iter().Where(pm => pm.StripeCustomerId == stripeCustomerId).Take(StripeMaxQueryRows).ToList());

    [SpacetimeDB.Procedure]
    public static Result<List<StripePayment>, string> StripeListPaymentsByUserId(ProcedureContext ctx, string userId) =>
        StripeAdminRead(ctx, db => db.StripePayment.Iter().Where(pm => pm.UserId == userId).Take(StripeMaxQueryRows).ToList());

    [SpacetimeDB.Procedure]
    public static Result<List<StripePayment>, string> StripeListPaymentsByOrgId(ProcedureContext ctx, string orgId) =>
        StripeAdminRead(ctx, db => db.StripePayment.Iter().Where(pm => pm.OrgId == orgId).Take(StripeMaxQueryRows).ToList());

    [SpacetimeDB.Procedure]
    public static Result<List<StripeInvoice>, string> StripeListInvoices(ProcedureContext ctx, string stripeCustomerId) =>
        StripeAdminRead(ctx, db => db.StripeInvoice.StripeCustomerId.Filter(stripeCustomerId).Take(StripeMaxQueryRows).ToList());

    [SpacetimeDB.Procedure]
    public static Result<List<StripeInvoice>, string> StripeListInvoicesByOrgId(ProcedureContext ctx, string orgId) =>
        StripeAdminRead(ctx, db => db.StripeInvoice.Iter().Where(i => i.OrgId == orgId).Take(StripeMaxQueryRows).ToList());

    [SpacetimeDB.Procedure]
    public static Result<List<StripeInvoice>, string> StripeListInvoicesByUserId(ProcedureContext ctx, string userId) =>
        StripeAdminRead(ctx, db => db.StripeInvoice.Iter().Where(i => i.UserId == userId).Take(StripeMaxQueryRows).ToList());

    [SpacetimeDB.Procedure]
    public static Result<StripeCheckoutSession?, string> StripeGetCheckoutSession(ProcedureContext ctx, string stripeCheckoutSessionId) =>
        StripeAdminRead(ctx, db => db.StripeCheckoutSession.StripeCheckoutSessionId.Find(stripeCheckoutSessionId));

    [SpacetimeDB.Procedure]
    public static Result<List<StripeCheckoutSession>, string> StripeListCheckoutSessions(ProcedureContext ctx, string stripeCustomerId) =>
        StripeAdminRead(ctx, db => db.StripeCheckoutSession.Iter().Where(s => s.StripeCustomerId == stripeCustomerId).Take(StripeMaxQueryRows).ToList());

    [SpacetimeDB.Procedure]
    public static Result<StripeWebhookEvent?, string> StripeGetWebhookEvent(ProcedureContext ctx, string eventId) =>
        StripeAdminRead(ctx, db => db.StripeWebhookEvent.EventId.Find(eventId));
}
