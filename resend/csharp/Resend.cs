// Resend.cs: send email through Resend and track delivery, for C# modules.
//
// Unofficial C# port of @spacetimedb/resend (SpacetimeDB PR #5903, commit
// ae25f1da24b1723e28acd0038e75e598f33952c3, package spacetime-resend-ts). Licensed Apache-2.0,
// like the original. Original design and behaviour by Clockwork Labs; this port is by Tinker
// (Gazz-Stripbolt).
//
// Same private tables (resend_email, resend_delivery_event, resend_webhook_event, resend_config,
// resend_admin_identity), validation limits, error codes, status ordering and webhook handling
// as the TS package. Needs Crypto.cs from this repo (Svix signatures).
//
//     public static partial class Module
//     {
//         [Reducer(ReducerKind.Init)]
//         public static void Init(ReducerContext ctx) => ResendInstall(ctx);
//
//         [HttpRouter]
//         public static Router Routes() => Router.New().Post("/webhook/resend", Handlers.ResendWebhook);
//
//         // Your own send procedure decides who may send to whom, then:
//         //   ResendSendEmailRequest(ctx, new ResendSendEmailArgs { To = [to], Subject = "Hi", Html = "<p>…</p>" });
//     }
//
// Changes from the TypeScript original:
// - Names: a drop-in shares your module's namespace, so generic procedures and reducers get a
//   resend_ prefix (send_email → resend_send_email, add_admin_identity →
//   resend_add_admin_identity, …) and types a Resend prefix (EmailStatus → ResendEmailStatus,
//   WebhookEventStatus → ResendWebhookEventStatus, SendEmailResult → ResendSendEmailResult).
// - Procedures return Result<T, string> instead of throwing.
// - resend_list_emails_by_user_id / _by_org_id scan the table (the Rust port can't filter an
//   index on an optional column, and both ports behave the same).
// - Payload validation is hand-written (no valibot): Failed rows carry differently worded
//   messages. Event timestamps accept ISO 8601 / RFC 3339 forms.
// - JSON the module writes (send bodies, stored tags) is serialized like JS JSON.stringify,
//   including its key order (integer-like keys first).
// - The API base URL can be overridden by implementing the optional partial method
//   ResendOverrideApiBase (tests point it at a mock). Default https://api.resend.com.

#pragma warning disable STDB_UNSTABLE
#nullable enable

using System.Globalization;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using SpacetimeDB;
using HttpMethod = SpacetimeDB.HttpMethod;

[SpacetimeDB.Type]
public enum ResendWebhookEventStatus
{
    Received,
    Processed,
    Ignored,
    Failed,
}

[SpacetimeDB.Type]
public enum ResendEmailStatus
{
    Queued,
    Sent,
    Delivered,
    DeliveryDelayed,
    Bounced,
    Failed,
    Cancelled,
}

[SpacetimeDB.Type]
public partial struct ResendSendEmailResult
{
    public string ResendId;
}

[SpacetimeDB.Type]
public partial struct ResendConfigStatus
{
    public bool IsConfigured;
    public bool HasWebhookSecret;
    public string? DefaultFrom;
    public ushort ApiKeyLength;
}

[SpacetimeDB.Type]
public partial struct ResendApiRequestResult
{
    public ushort Status;
    public string Body;
}

/// <summary>Arguments for ResendSendEmailRequest (the TS SendEmailArgs).</summary>
public sealed class ResendSendEmailArgs
{
    public string? From { get; init; }
    public List<string> To { get; init; } = [];
    public string Subject { get; init; } = "";
    public string? Html { get; init; }
    public string? Text { get; init; }
    public List<string>? Cc { get; init; }
    public List<string>? Bcc { get; init; }
    public List<string>? ReplyTo { get; init; }
    public string? TagsJson { get; init; }
    public string? HeadersJson { get; init; }
    public string? ScheduledAt { get; init; }
    public string? IdempotencyKey { get; init; }
}

/// <summary>A transaction to read or write in, from a reducer, procedure or HTTP handler.</summary>
public readonly record struct ResendTx(Local Db, Timestamp Timestamp);

/// <summary>Thrown inside ResendTx work to report a rejection with an HTTP status.</summary>
public readonly record struct ResendWebhookRejection(ushort Status, string Code);

public static partial class Module
{
    /// <summary>The resend.* codes, as in the TS errors object.</summary>
    public static class ResendErrors
    {
        public const string NotAuthorized = "resend.not_authorized";
        public const string CannotRemoveLastAdmin = "resend.cannot_remove_last_admin";
        public const string ConfigNotSet = "resend.config_not_set";
        public const string RequestInvalid = "resend.request_invalid";
        public const string RequestPathInvalid = "resend.request_path_invalid";
        public const string RequestMethodInvalid = "resend.request_method_invalid";
        public const string RequestBodyTooLarge = "resend.request_body_too_large";
        public const string IdempotencyKeyTooLong = "resend.idempotency_key_too_long";
        public const string SendEmailInvalidInput = "resend.send_email_invalid_input";
        public const string SendEmailNoRecipients = "resend.send_email_no_recipients";
        public const string SendEmailTooManyRecipients = "resend.send_email_too_many_recipients";
        public const string SendEmailInvalidFrom = "resend.send_email_invalid_from";
        public const string SendEmailInvalidSubject = "resend.send_email_invalid_subject";
        public const string SendEmailMissingContent = "resend.send_email_missing_content";
        public const string SendEmailHtmlTooLarge = "resend.send_email_html_too_large";
        public const string SendEmailTextTooLarge = "resend.send_email_text_too_large";
        public const string SendEmailInvalidSchedule = "resend.send_email_invalid_schedule";
        public const string SendEmailMissingFrom = "resend.send_email_missing_from";
        public const string SendEmailFailed = "resend.send_email_failed";
        public const string SendEmailInvalidResponse = "resend.send_email_invalid_response";
        public const string ToTooMany = "resend.to_too_many";
        public const string ToInvalidAddress = "resend.to_invalid_address";
        public const string CcTooMany = "resend.cc_too_many";
        public const string CcInvalidAddress = "resend.cc_invalid_address";
        public const string BccTooMany = "resend.bcc_too_many";
        public const string BccInvalidAddress = "resend.bcc_invalid_address";
        public const string ReplyToTooMany = "resend.reply_to_too_many";
        public const string ReplyToInvalidAddress = "resend.reply_to_invalid_address";
        public const string TagsTooLarge = "resend.tags_too_large";
        public const string TagsInvalidJson = "resend.tags_invalid_json";
        public const string HeadersTooLarge = "resend.headers_too_large";
        public const string HeadersInvalidJson = "resend.headers_invalid_json";
        public const string CancelEmailFailed = "resend.cancel_email_failed";
        public const string WebhookMethodNotAllowed = "resend.webhook_method_not_allowed";
        public const string WebhookMetadataInvalid = "resend.webhook_metadata_invalid";
        public const string WebhookMetadataMismatch = "resend.webhook_metadata_mismatch";
        public const string WebhookPayloadTooLarge = "resend.webhook_payload_too_large";
        public const string WebhookHeaderTooLarge = "resend.webhook_header_too_large";
        public const string WebhookSecretNotConfigured = "resend.webhook_secret_not_configured";
        public const string WebhookSignatureMismatch = "resend.webhook_signature_mismatch";
        public const string WebhookPayloadInvalid = "resend.webhook_payload_invalid";
        public const string WebhookEventNotFound = "resend.webhook_event_not_found";
    }

    const int ResendMaxPathLength = 2048;
    const int ResendMaxJsonBodyLength = 256 * 1024;
    const int ResendMaxIdempotencyKeyLength = 256;
    const int ResendMaxQueryRows = 1000;
    const int ResendMaxWebhookBodyLength = 1024 * 1024;
    const int ResendMaxWebhookHeaderLength = 8192;
    const int ResendMaxWebhookMetadataLength = 255;

    /// <summary>Optional: implement to point API calls somewhere else (tests use a mock).</summary>
    static partial void ResendOverrideApiBase(ref string apiBase);

    static string ResendApiBase()
    {
        var b = "https://api.resend.com";
        ResendOverrideApiBase(ref b);
        return b;
    }

    static bool ResendIsArrayIndex(string k) =>
        k.Length > 0 && k.All(c => c is >= '0' and <= '9') && (k == "0" || k[0] != '0')
        && ulong.TryParse(k, NumberStyles.None, CultureInfo.InvariantCulture, out var n) && n < 4_294_967_295;

    /// <summary>A JSON string literal escaped as JSON.stringify does (non-ASCII kept raw).</summary>
    static string ResendJsString(string s)
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

    static string ResendJsNumber(JsonValue v)
    {
        if (v.TryGetValue<long>(out var l)) return l.ToString(CultureInfo.InvariantCulture);
        var d = v.GetValue<double>();
        if (Math.Abs(d) < 1e21 && Math.Floor(d) == d) return d.ToString("F0", CultureInfo.InvariantCulture);
        return d.ToString("R", CultureInfo.InvariantCulture);
    }

    /// <summary>JSON.stringify as JS writes it: integer-like keys first, non-ASCII raw.</summary>
    static string ResendJsJson(JsonNode? v) => v switch
    {
        null => "null",
        JsonObject o => "{" + string.Join(",", o.Where(kv => ResendIsArrayIndex(kv.Key)).OrderBy(kv => ulong.Parse(kv.Key, CultureInfo.InvariantCulture))
            .Concat(o.Where(kv => !ResendIsArrayIndex(kv.Key)))
            .Select(kv => ResendJsString(kv.Key) + ":" + ResendJsJson(kv.Value))) + "}",
        JsonArray a => "[" + string.Join(",", a.Select(ResendJsJson)) + "]",
        JsonValue x => x.GetValueKind() switch
        {
            JsonValueKind.String => ResendJsString(x.GetValue<string>()),
            JsonValueKind.Number => ResendJsNumber(x),
            JsonValueKind.True => "true",
            JsonValueKind.False => "false",
            _ => "null",
        },
        _ => "null",
    };

    // -----------------------------------------------------------------------
    // Tables
    // -----------------------------------------------------------------------

    [SpacetimeDB.Table(Accessor = "ResendEmail")]
    public partial struct ResendEmail
    {
        [SpacetimeDB.PrimaryKey]
        public string ResendId;
        public string FromAddress;
        public string ToAddressesJson;
        public string? Subject;
        [SpacetimeDB.Index.BTree]
        public ResendEmailStatus Status;
        public string? LastError;
        public Timestamp? BouncedAt;
        public string? BounceJson;
        public Timestamp? FailedAt;
        public string? FailureReason;
        public bool Complained;
        public Timestamp? ComplainedAt;
        public bool Opened;
        public Timestamp? OpenedAt;
        public bool Clicked;
        public Timestamp? ClickedAt;
        public Timestamp? DeliveredAt;
        public Timestamp? SentAt;
        public string? Html;
        public string? Text;
        public string? TagsJson;
        [SpacetimeDB.Index.BTree]
        public string? UserId;
        [SpacetimeDB.Index.BTree]
        public string? OrgId;
        public Timestamp CreatedAt;
        public Timestamp UpdatedAt;
        public Timestamp? StatusUpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "ResendDeliveryEvent")]
    public partial struct ResendDeliveryEvent
    {
        [SpacetimeDB.PrimaryKey]
        public string EventId;
        [SpacetimeDB.Index.BTree]
        public string ResendId;
        public string EventType;
        public string CreatedAtIso;
        public string? DetailJson;
        public Timestamp InsertedAt;
    }

    /// <summary>Private: raw payloads and signature headers are operational data.</summary>
    [SpacetimeDB.Table(Accessor = "ResendWebhookEvent")]
    public partial struct ResendWebhookEvent
    {
        [SpacetimeDB.PrimaryKey]
        public string EventId;
        public string EventType;
        public string PayloadJson;
        public string? SignatureHeader;
        public string? TimestampHeader;
        [SpacetimeDB.Index.BTree]
        public ResendWebhookEventStatus Status;
        public string? ErrorMessage;
        public Timestamp ReceivedAt;
        public Timestamp? ProcessedAt;
    }

    /// <summary>Private singleton; secrets never leak through subscriptions.</summary>
    [SpacetimeDB.Table(Accessor = "ResendConfig")]
    public partial struct ResendConfig
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public string ApiKey;
        public string? WebhookSigningSecret;
        public string? DefaultFrom;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "ResendAdminIdentity")]
    public partial struct ResendAdminIdentity
    {
        [SpacetimeDB.PrimaryKey]
        public Identity Identity;
        public Timestamp AddedAt;
    }

    // -----------------------------------------------------------------------
    // Admins and config
    // -----------------------------------------------------------------------

    /// <summary>Call from your Init reducer: makes the publishing identity the first admin.</summary>
    public static void ResendInstall(ReducerContext ctx)
    {
        if (ctx.Db.ResendAdminIdentity.Identity.Find(ctx.Sender) is null)
        {
            ctx.Db.ResendAdminIdentity.Insert(new ResendAdminIdentity { Identity = ctx.Sender, AddedAt = ctx.Timestamp });
        }
    }

    public static bool ResendIsAdmin(Local db, Identity who) => db.ResendAdminIdentity.Identity.Find(who) is not null;

    static bool ResendProcedureIsAdmin(ProcedureContext ctx)
    {
        var sender = ctx.Sender;
        return ctx.WithTx(tx => ResendIsAdmin(tx.Db, sender));
    }

    static Result<T, string> ResendErr<T>(string e) => Result<T, string>.Err(e);

    [SpacetimeDB.Procedure]
    public static Result<Unit, string> ResendAddAdminIdentity(ProcedureContext ctx, Identity identity)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<Unit>(ResendErrors.NotAuthorized);
        var now = ctx.Timestamp;
        ctx.WithTx(tx =>
        {
            if (tx.Db.ResendAdminIdentity.Identity.Find(identity) is null)
            {
                tx.Db.ResendAdminIdentity.Insert(new ResendAdminIdentity { Identity = identity, AddedAt = now });
            }
            return 0;
        });
        return Result<Unit, string>.Ok(default);
    }

    [SpacetimeDB.Procedure]
    public static Result<Unit, string> ResendRemoveAdminIdentity(ProcedureContext ctx, Identity identity)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<Unit>(ResendErrors.NotAuthorized);
        var error = ctx.WithTx(tx =>
        {
            if (tx.Db.ResendAdminIdentity.Identity.Find(identity) is null) return null;
            if (tx.Db.ResendAdminIdentity.Count <= 1) return ResendErrors.CannotRemoveLastAdmin;
            tx.Db.ResendAdminIdentity.Identity.Delete(identity);
            return (string?)null;
        });
        return error is null ? Result<Unit, string>.Ok(default) : ResendErr<Unit>(error);
    }

    /// <summary>The stored config, or throws resend.config_not_set.</summary>
    public static ResendConfig ResendLoadConfig(Local db) =>
        db.ResendConfig.Singleton.Find(true)
        ?? throw new Exception($"{ResendErrors.ConfigNotSet}: call set_resend_config(...) first");

    /// <summary>Admins only. null keeps the stored webhook secret / default sender.</summary>
    [SpacetimeDB.Procedure]
    public static Result<Unit, string> SetResendConfig(ProcedureContext ctx, string apiKey, string? webhookSigningSecret, string? defaultFrom)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<Unit>(ResendErrors.NotAuthorized);
        var now = ctx.Timestamp;
        ctx.WithTx(tx =>
        {
            var existing = tx.Db.ResendConfig.Singleton.Find(true);
            var row = new ResendConfig
            {
                Singleton = true,
                ApiKey = apiKey,
                WebhookSigningSecret = webhookSigningSecret ?? existing?.WebhookSigningSecret,
                DefaultFrom = defaultFrom ?? existing?.DefaultFrom,
                UpdatedAt = now,
            };
            if (existing is not null) tx.Db.ResendConfig.Singleton.Update(row);
            else tx.Db.ResendConfig.Insert(row);
            return 0;
        });
        return Result<Unit, string>.Ok(default);
    }

    [SpacetimeDB.Procedure]
    public static Result<ResendConfigStatus, string> GetResendConfigStatus(ProcedureContext ctx)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<ResendConfigStatus>(ResendErrors.NotAuthorized);
        return Result<ResendConfigStatus, string>.Ok(ctx.WithTx(tx =>
            tx.Db.ResendConfig.Singleton.Find(true) is { } row
                ? new ResendConfigStatus
                {
                    IsConfigured = true,
                    HasWebhookSecret = row.WebhookSigningSecret is not null,
                    DefaultFrom = row.DefaultFrom,
                    ApiKeyLength = (ushort)Math.Min(row.ApiKey.Length, ushort.MaxValue),
                }
                : new ResendConfigStatus { IsConfigured = false, HasWebhookSecret = false, DefaultFrom = null, ApiKeyLength = 0 }));
    }

    // -----------------------------------------------------------------------
    // Validation helpers
    // -----------------------------------------------------------------------

    static bool ResendHasControlCharacter(string s)
    {
        foreach (var c in s) if (c <= 0x1f || c == 0x7f) return true;
        return false;
    }

    /// <summary>JS s.replace(/\s+/g, ' ').</summary>
    static string ResendCollapseWhitespace(string s)
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

    /// <summary>JS slice(0, n), without splitting a surrogate pair.</summary>
    static string ResendSlice(string s, int n)
    {
        if (s.Length <= n) return s;
        if (n > 0 && char.IsHighSurrogate(s[n - 1])) n--;
        return s[..n];
    }

    static string? ResendValidateAddressList(List<string>? values, string tooMany, string invalid)
    {
        if (values is null) return null;
        if (values.Count > 100) return tooMany;
        foreach (var v in values)
        {
            if (v.Length == 0 || v.Length > 320 || ResendHasControlCharacter(v)) return invalid;
        }
        return null;
    }

    static string? ResendValidateJson(string? value, string tooLarge, string invalid, int maxLength)
    {
        if (value is null) return null;
        if (value.Length > maxLength) return tooLarge;
        try
        {
            return JsonNode.Parse(value) is JsonObject or JsonArray ? null : invalid;
        }
        catch (Exception)
        {
            return invalid;
        }
    }

    /// <summary>The TS validateEmailInput: limits and header-injection checks before any HTTP. Null when valid.</summary>
    public static string? ResendValidateEmailInput(ResendSendEmailArgs a)
    {
        if (a.To.Count == 0) return ResendErrors.SendEmailNoRecipients;
        var e = ResendValidateAddressList(a.To, ResendErrors.ToTooMany, ResendErrors.ToInvalidAddress)
            ?? ResendValidateAddressList(a.Cc, ResendErrors.CcTooMany, ResendErrors.CcInvalidAddress)
            ?? ResendValidateAddressList(a.Bcc, ResendErrors.BccTooMany, ResendErrors.BccInvalidAddress)
            ?? ResendValidateAddressList(a.ReplyTo, ResendErrors.ReplyToTooMany, ResendErrors.ReplyToInvalidAddress);
        if (e is not null) return e;
        if (a.To.Count + (a.Cc?.Count ?? 0) + (a.Bcc?.Count ?? 0) > 100) return ResendErrors.SendEmailTooManyRecipients;
        if (a.From is { } from && (from.Length == 0 || from.Length > 320 || ResendHasControlCharacter(from)))
        {
            return ResendErrors.SendEmailInvalidFrom;
        }
        if (a.Subject.Length == 0 || a.Subject.Length > 998 || ResendHasControlCharacter(a.Subject))
        {
            return ResendErrors.SendEmailInvalidSubject;
        }
        if (a.Html is null && a.Text is null) return ResendErrors.SendEmailMissingContent;
        if ((a.Html?.Length ?? 0) > 200_000) return ResendErrors.SendEmailHtmlTooLarge;
        if ((a.Text?.Length ?? 0) > 200_000) return ResendErrors.SendEmailTextTooLarge;
        e = ResendValidateJson(a.TagsJson, ResendErrors.TagsTooLarge, ResendErrors.TagsInvalidJson, 16_384)
            ?? ResendValidateJson(a.HeadersJson, ResendErrors.HeadersTooLarge, ResendErrors.HeadersInvalidJson, 16_384);
        if (e is not null) return e;
        var scheduled = a.ScheduledAt ?? "";
        if (scheduled.Length > 128 || ResendHasControlCharacter(scheduled)) return ResendErrors.SendEmailInvalidSchedule;
        return null;
    }

    /// <summary>A validated request to the Resend API (the TS buildResendHttpRequest).</summary>
    public sealed record ResendHttpRequest(string Method, string Url, List<(string Name, string Value)> Headers, string? Body);

    /// <summary>Throws the resend.request_* codes on invalid input.</summary>
    public static ResendHttpRequest ResendBuildHttpRequest(string method, string path, string apiKey, string? jsonBody, string? idempotencyKey)
    {
        method = method.Trim().ToUpperInvariant();
        if (method is not ("GET" or "POST" or "PATCH" or "DELETE")) throw new Exception(ResendErrors.RequestMethodInvalid);
        path = path.Trim();
        if (!path.StartsWith('/') || path.StartsWith("//", StringComparison.Ordinal) || path.Contains('\\') || path.Contains('#')
            || path.Length > ResendMaxPathLength || ResendHasControlCharacter(path))
        {
            throw new Exception(ResendErrors.RequestPathInvalid);
        }
        var body = string.IsNullOrEmpty(jsonBody) ? null : jsonBody;
        if (body is not null && body.Length > ResendMaxJsonBodyLength) throw new Exception(ResendErrors.RequestBodyTooLarge);
        if (!string.IsNullOrEmpty(idempotencyKey) && idempotencyKey.Length > ResendMaxIdempotencyKeyLength)
        {
            throw new Exception(ResendErrors.IdempotencyKeyTooLong);
        }
        var headers = new List<(string, string)> { ("Authorization", $"Bearer {apiKey}"), ("Accept", "application/json") };
        if (body is not null) headers.Add(("Content-Type", "application/json"));
        if (!string.IsNullOrEmpty(idempotencyKey)) headers.Add(("Idempotency-Key", idempotencyKey));
        return new ResendHttpRequest(method, ResendApiBase() + path, headers, body);
    }

    static ResendApiRequestResult ResendCall(ProcedureContext ctx, string method, string path, string apiKey, string? jsonBody, string? idempotencyKey)
    {
        var r = ResendBuildHttpRequest(method, path, apiKey, jsonBody, idempotencyKey);
        var request = new HttpRequest
        {
            Uri = r.Url,
            Method = new HttpMethod(r.Method),
            Headers = r.Headers.Select(h => new HttpHeader(h.Name, h.Value)).ToList(),
            Body = r.Body is null ? HttpBody.Empty : HttpBody.FromString(r.Body),
        };
        ResendApiRequestResult? result = null;
        string? error = null;
        ctx.Http.Send(request).Match(
            response =>
            {
                result = new ResendApiRequestResult { Status = response.StatusCode, Body = response.Body.ToStringUtf8Lossy() };
                return 0;
            },
            e =>
            {
                error = e.ToString();
                return 0;
            });
        return result ?? throw new Exception(error ?? ResendErrors.RequestInvalid);
    }

    /// <summary>resend.&lt;op&gt;_failed:&lt;status&gt;[:name=…|msg=…] unless the response is 2xx.</summary>
    static void ResendEnsureOk(ResendApiRequestResult response, string prefix)
    {
        if (response.Status is >= 200 and < 300) return;
        throw new Exception($"{prefix}:{response.Status}{ResendErrorSuffix(response.Body)}");
    }

    static string ResendErrorSuffix(string body)
    {
        JsonNode? parsed = null;
        try { parsed = JsonNode.Parse(body); } catch (Exception) { }
        if (parsed is JsonObject o)
        {
            var name = o["name"];
            var message = o["message"];
            var hasCode = o.TryGetPropertyValue("statusCode", out var code);
            bool IsString(JsonNode? n) => n is JsonValue v && v.GetValueKind() == JsonValueKind.String;
            var shapeOk = (!o.ContainsKey("name") || IsString(name))
                && (!o.ContainsKey("message") || IsString(message))
                && (!hasCode || code is null || (code is JsonValue cv && cv.GetValueKind() == JsonValueKind.Number));
            if (shapeOk)
            {
                var parts = new List<string>();
                if (name?.GetValue<string>() is { Length: > 0 } n) parts.Add($"name={n}");
                if (message?.GetValue<string>() is { Length: > 0 } m) parts.Add($"msg={ResendSlice(ResendCollapseWhitespace(m), 240)}");
                if (parts.Count > 0) return ":" + string.Join("|", parts);
            }
        }
        var compact = ResendCollapseWhitespace(body).Trim();
        return compact.Length == 0 ? "" : $":body={ResendSlice(compact, 240)}";
    }

    // -----------------------------------------------------------------------
    // Email rows
    // -----------------------------------------------------------------------

    /// <summary>Sparse fields for ResendUpsertEmail: null keeps the stored value.</summary>
    sealed class ResendEmailUpsert
    {
        public string ResendId = "";
        public string FromAddress = "";
        public string ToAddressesJson = "";
        public string? Subject, Html, Text, LastError, BounceJson, FailureReason, TagsJson, UserId, OrgId;
        public ResendEmailStatus? Status;
        public Timestamp? StatusUpdatedAt, BouncedAt, FailedAt, ComplainedAt, OpenedAt, ClickedAt, DeliveredAt, SentAt;
        public bool Complained, Opened, Clicked;
    }

    static int ResendStatusOrder(ResendEmailStatus s) => s switch
    {
        ResendEmailStatus.Queued => 0,
        ResendEmailStatus.Sent => 1,
        ResendEmailStatus.DeliveryDelayed => 2,
        ResendEmailStatus.Delivered => 3,
        ResendEmailStatus.Cancelled => 4,
        ResendEmailStatus.Failed => 5,
        _ => 6,
    };

    static void ResendUpsertEmail(ResendTx tx, Timestamp now, ResendEmailUpsert a)
    {
        var existing = tx.Db.ResendEmail.ResendId.Find(a.ResendId);
        var statusTime = a.StatusUpdatedAt ?? now;
        // Late webhooks and send responses can't undo a later delivery stage.
        var replace = a.Status is { } s && (existing is not { } e0
            || (ResendStatusOrder(s) >= ResendStatusOrder(e0.Status)
                && (e0.Status == ResendEmailStatus.Queued || e0.StatusUpdatedAt is not { } t
                    || statusTime.MicrosecondsSinceUnixEpoch >= t.MicrosecondsSinceUnixEpoch)));
        var e = existing;
        // Error and bounce details describe the status that produced them.
        string? Detail(string? v, string? old) => replace && v is not null ? v : old;
        Timestamp? DetailT(Timestamp? v, Timestamp? old) => replace && v is not null ? v : old;
        var row = new ResendEmail
        {
            ResendId = a.ResendId,
            FromAddress = a.FromAddress,
            ToAddressesJson = a.ToAddressesJson,
            Subject = a.Subject ?? e?.Subject,
            Html = a.Html ?? e?.Html,
            Text = a.Text ?? e?.Text,
            Status = replace ? a.Status!.Value : e?.Status ?? ResendEmailStatus.Queued,
            StatusUpdatedAt = replace ? statusTime : e?.StatusUpdatedAt,
            LastError = Detail(a.LastError, e?.LastError),
            BouncedAt = DetailT(a.BouncedAt, e?.BouncedAt),
            BounceJson = Detail(a.BounceJson, e?.BounceJson),
            FailedAt = DetailT(a.FailedAt, e?.FailedAt),
            FailureReason = Detail(a.FailureReason, e?.FailureReason),
            Complained = a.Complained || (e?.Complained ?? false),
            ComplainedAt = a.ComplainedAt ?? e?.ComplainedAt,
            Opened = a.Opened || (e?.Opened ?? false),
            OpenedAt = a.OpenedAt ?? e?.OpenedAt,
            Clicked = a.Clicked || (e?.Clicked ?? false),
            ClickedAt = a.ClickedAt ?? e?.ClickedAt,
            DeliveredAt = a.DeliveredAt ?? e?.DeliveredAt,
            SentAt = a.SentAt ?? e?.SentAt,
            TagsJson = a.TagsJson ?? e?.TagsJson,
            UserId = a.UserId ?? e?.UserId,
            OrgId = a.OrgId ?? e?.OrgId,
            CreatedAt = e?.CreatedAt ?? now,
            UpdatedAt = now,
        };
        if (existing is not null) tx.Db.ResendEmail.ResendId.Update(row);
        else tx.Db.ResendEmail.Insert(row);
    }

    /// <summary>userId / orgId from send-API tags ([{ "name": "userId", "value": "u_1" }]).</summary>
    static (string? UserId, string? OrgId) ResendTagFieldsFromSendTags(string? tagsJson)
    {
        if (string.IsNullOrEmpty(tagsJson)) return (null, null);
        JsonNode? parsed;
        try { parsed = JsonNode.Parse(tagsJson); } catch (Exception) { return (null, null); }
        if (parsed is not JsonArray items) return (null, null);
        var pairs = new List<(string, string)>();
        foreach (var item in items)
        {
            if (item is not JsonObject o || ResendString(o, "name") is not { } name || ResendString(o, "value") is not { } value)
            {
                return (null, null);
            }
            pairs.Add((name, value));
        }
        string? Find(string k) => pairs.Where(p => p.Item1 == k).Select(p => p.Item2).FirstOrDefault();
        return (Find("userId"), Find("orgId"));
    }

    static string? ResendString(JsonObject o, string key) =>
        o[key] is JsonValue v && v.GetValueKind() == JsonValueKind.String ? v.GetValue<string>() : null;

    static JsonArray ResendStringArray(IEnumerable<string> values)
    {
        var a = new JsonArray();
        foreach (var v in values) a.Add((JsonNode?)JsonValue.Create(v));
        return a;
    }

    static string ResendBuildSendEmailBody(ResendSendEmailArgs a, string from)
    {
        var body = new JsonObject
        {
            ["from"] = JsonValue.Create(from),
            ["to"] = ResendStringArray(a.To),
            ["subject"] = JsonValue.Create(a.Subject),
        };
        if (a.Html is not null) body["html"] = JsonValue.Create(a.Html);
        if (a.Text is not null) body["text"] = JsonValue.Create(a.Text);
        if (a.Cc is { Count: > 0 }) body["cc"] = ResendStringArray(a.Cc);
        if (a.Bcc is { Count: > 0 }) body["bcc"] = ResendStringArray(a.Bcc);
        if (a.ReplyTo is { Count: > 0 }) body["reply_to"] = ResendStringArray(a.ReplyTo);
        if (a.ScheduledAt is not null) body["scheduled_at"] = JsonValue.Create(a.ScheduledAt);
        if (a.TagsJson is not null)
        {
            try { body["tags"] = JsonNode.Parse(a.TagsJson); } catch (Exception) { }
        }
        if (a.HeadersJson is not null)
        {
            try { body["headers"] = JsonNode.Parse(a.HeadersJson); } catch (Exception) { }
        }
        return ResendJsJson(body);
    }

    /// <summary>
    /// Send an email (no admin check: call it from your own procedure after deciding who may send to
    /// whom). Records a Queued row on success. Throws the resend.* codes.
    /// </summary>
    public static ResendSendEmailResult ResendSendEmailRequest(ProcedureContext ctx, ResendSendEmailArgs args)
    {
        if (ResendValidateEmailInput(args) is { } invalid) throw new Exception(invalid);
        var cfg = ctx.WithTx(tx => ResendLoadConfig(tx.Db));
        var from = args.From ?? cfg.DefaultFrom ?? throw new Exception(ResendErrors.SendEmailMissingFrom);
        var response = ResendCall(ctx, "POST", "/emails", cfg.ApiKey, ResendBuildSendEmailBody(args, from), args.IdempotencyKey);
        ResendEnsureOk(response, ResendErrors.SendEmailFailed);
        JsonNode? parsed;
        try { parsed = JsonNode.Parse(response.Body); }
        catch (Exception) { throw new Exception(ResendErrors.SendEmailInvalidResponse); }
        if (parsed is null) throw new Exception(ResendErrors.SendEmailInvalidResponse);
        if (parsed is not JsonObject obj) throw new Exception($"{ResendErrors.SendEmailInvalidResponse}:Invalid type: Expected Object");
        var resendId = obj.ContainsKey("id")
            ? ResendString(obj, "id") ?? throw new Exception($"{ResendErrors.SendEmailInvalidResponse}:Invalid type: Expected string at id")
            : throw new Exception($"{ResendErrors.SendEmailInvalidResponse}:Invalid key: Expected \"id\" but received undefined at id");
        var now = ctx.Timestamp;
        var (userId, orgId) = ResendTagFieldsFromSendTags(args.TagsJson);
        var toJson = ResendJsJson(ResendStringArray(args.To));
        ctx.WithTx(tx =>
        {
            ResendUpsertEmail(new ResendTx(tx.Db, tx.Timestamp), now, new ResendEmailUpsert
            {
                ResendId = resendId,
                FromAddress = from,
                ToAddressesJson = toJson,
                Subject = args.Subject,
                Html = args.Html,
                Text = args.Text,
                Status = ResendEmailStatus.Queued,
                TagsJson = args.TagsJson,
                UserId = userId,
                OrgId = orgId,
            });
            return 0;
        });
        return new ResendSendEmailResult { ResendId = resendId };
    }

    static Result<T, string> ResendTry<T>(Func<T> f)
    {
        try { return Result<T, string>.Ok(f()); }
        catch (Exception e) { return Result<T, string>.Err(e.Message); }
    }

    /// <summary>Admins only.</summary>
    [SpacetimeDB.Procedure]
    public static Result<ResendSendEmailResult, string> ResendSendEmail(
        ProcedureContext ctx, string? from, List<string> to, string subject, string? html, string? text, List<string>? cc,
        List<string>? bcc, List<string>? replyTo, string? tagsJson, string? headersJson, string? scheduledAt, string? idempotencyKey)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<ResendSendEmailResult>(ResendErrors.NotAuthorized);
        return ResendTry(() => ResendSendEmailRequest(ctx, new ResendSendEmailArgs
        {
            From = from, To = to, Subject = subject, Html = html, Text = text, Cc = cc, Bcc = bcc, ReplyTo = replyTo,
            TagsJson = tagsJson, HeadersJson = headersJson, ScheduledAt = scheduledAt, IdempotencyKey = idempotencyKey,
        }));
    }

    /// <summary>Admins only.</summary>
    [SpacetimeDB.Procedure]
    public static Result<Unit, string> ResendCancelEmail(ProcedureContext ctx, string resendId)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<Unit>(ResendErrors.NotAuthorized);
        return ResendTry(() =>
        {
            var cfg = ctx.WithTx(tx => ResendLoadConfig(tx.Db));
            var response = ResendCall(ctx, "POST", $"/emails/{resendId}/cancel", cfg.ApiKey, null, null);
            ResendEnsureOk(response, ResendErrors.CancelEmailFailed);
            var now = ctx.Timestamp;
            ctx.WithTx(tx =>
            {
                if (tx.Db.ResendEmail.ResendId.Find(resendId) is { } e)
                {
                    tx.Db.ResendEmail.ResendId.Update(e with { Status = ResendEmailStatus.Cancelled, StatusUpdatedAt = now, UpdatedAt = now });
                }
                return 0;
            });
            return default(Unit);
        });
    }

    /// <summary>Admins only: an authenticated request to a relative api.resend.com path.</summary>
    [SpacetimeDB.Procedure]
    public static Result<ResendApiRequestResult, string> ResendApiRequest(ProcedureContext ctx, string method, string path, string? jsonBody, string? idempotencyKey)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<ResendApiRequestResult>(ResendErrors.NotAuthorized);
        return ResendTry(() =>
        {
            var cfg = ctx.WithTx(tx => ResendLoadConfig(tx.Db));
            return ResendCall(ctx, method, path, cfg.ApiKey, jsonBody, idempotencyKey);
        });
    }

    // -----------------------------------------------------------------------
    // Admin queries (at most 1,000 rows each)
    // -----------------------------------------------------------------------

    [SpacetimeDB.Procedure]
    public static Result<ResendEmail?, string> ResendGetEmail(ProcedureContext ctx, string resendId)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<ResendEmail?>(ResendErrors.NotAuthorized);
        return Result<ResendEmail?, string>.Ok(ctx.WithTx(tx => tx.Db.ResendEmail.ResendId.Find(resendId)));
    }

    [SpacetimeDB.Procedure]
    public static Result<ResendWebhookEvent?, string> ResendGetWebhookEvent(ProcedureContext ctx, string eventId)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<ResendWebhookEvent?>(ResendErrors.NotAuthorized);
        return Result<ResendWebhookEvent?, string>.Ok(ctx.WithTx(tx => tx.Db.ResendWebhookEvent.EventId.Find(eventId)));
    }

    [SpacetimeDB.Procedure]
    public static Result<List<ResendEmail>, string> ResendListEmailsByUserId(ProcedureContext ctx, string userId)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<List<ResendEmail>>(ResendErrors.NotAuthorized);
        return Result<List<ResendEmail>, string>.Ok(ctx.WithTx(tx =>
            tx.Db.ResendEmail.Iter().Where(e => e.UserId == userId).Take(ResendMaxQueryRows).ToList()));
    }

    [SpacetimeDB.Procedure]
    public static Result<List<ResendEmail>, string> ResendListEmailsByOrgId(ProcedureContext ctx, string orgId)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<List<ResendEmail>>(ResendErrors.NotAuthorized);
        return Result<List<ResendEmail>, string>.Ok(ctx.WithTx(tx =>
            tx.Db.ResendEmail.Iter().Where(e => e.OrgId == orgId).Take(ResendMaxQueryRows).ToList()));
    }

    [SpacetimeDB.Procedure]
    public static Result<List<ResendEmail>, string> ResendListEmailsByStatus(ProcedureContext ctx, ResendEmailStatus status)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<List<ResendEmail>>(ResendErrors.NotAuthorized);
        return Result<List<ResendEmail>, string>.Ok(ctx.WithTx(tx =>
            tx.Db.ResendEmail.Status.Filter(status).Take(ResendMaxQueryRows).ToList()));
    }

    [SpacetimeDB.Procedure]
    public static Result<List<ResendDeliveryEvent>, string> ResendListDeliveryEventsForEmail(ProcedureContext ctx, string resendId)
    {
        if (!ResendProcedureIsAdmin(ctx)) return ResendErr<List<ResendDeliveryEvent>>(ResendErrors.NotAuthorized);
        return Result<List<ResendDeliveryEvent>, string>.Ok(ctx.WithTx(tx =>
            tx.Db.ResendDeliveryEvent.ResendId.Filter(resendId).Take(ResendMaxQueryRows).ToList()));
    }

    // -----------------------------------------------------------------------
    // Webhooks
    // -----------------------------------------------------------------------

    static readonly string[] ResendHandledEventTypes =
    [
        "email.sent", "email.delivered", "email.delivery_delayed", "email.complained",
        "email.bounced", "email.opened", "email.clicked", "email.failed",
    ];

    /// <summary>type from a webhook payload, if it's a non-empty string in a JSON object.</summary>
    public static string? ResendParseEventType(string payloadJson)
    {
        try
        {
            return JsonNode.Parse(payloadJson) is JsonObject o && ResendString(o, "type") is { Length: > 0 } t ? t : null;
        }
        catch (Exception)
        {
            return null;
        }
    }

    sealed class ResendEmailEvent
    {
        public string Kind = "", CreatedAt = "", EmailId = "", From = "", Subject = "";
        public List<string> To = [];
        public JsonObject? Tags;
        public JsonObject? Detail;
        public string? BounceMessage, FailedReason;
    }

    static string ResendWantStr(JsonObject o, string key, string path) =>
        !o.ContainsKey(key) ? throw new FormatException($"Invalid key: Expected \"{key}\" but received undefined at {path}{key}")
        : ResendString(o, key) ?? throw new FormatException($"Invalid type: Expected string at {path}{key}");

    static void ResendOptStr(JsonObject o, string key, string path)
    {
        if (o.ContainsKey(key) && ResendString(o, key) is null) throw new FormatException($"Invalid type: Expected string at {path}{key}");
    }

    static JsonObject ResendWantObj(JsonObject o, string key, string path) =>
        !o.ContainsKey(key) ? throw new FormatException($"Invalid key: Expected \"{key}\" but received undefined at {path}{key}")
        : o[key] as JsonObject ?? throw new FormatException($"Invalid type: Expected Object at {path}{key}");

    static JsonObject ResendSubObject(JsonObject data, string key, string[] keys)
    {
        var o = ResendWantObj(data, key, "data.");
        var outObj = new JsonObject();
        foreach (var k in keys) outObj[k] = JsonValue.Create(ResendWantStr(o, k, $"data.{key}."));
        return outObj;
    }

    static ResendEmailEvent ResendParseEmailEvent(JsonNode? v)
    {
        if (v is not JsonObject o) throw new FormatException("Invalid type: Expected Object");
        var kind = ResendString(o, "type") is { } t && ResendHandledEventTypes.Contains(t) ? t : throw new FormatException("Invalid type at type");
        var ev = new ResendEmailEvent { Kind = kind, CreatedAt = ResendWantStr(o, "created_at", "") };
        var data = ResendWantObj(o, "data", "");
        ResendOptStr(data, "broadcast_id", "data.");
        ResendWantStr(data, "created_at", "data.");
        ev.EmailId = ResendWantStr(data, "email_id", "data.");
        ev.From = ResendWantStr(data, "from", "data.");
        if (!data.ContainsKey("to")) throw new FormatException("Invalid key: Expected \"to\" but received undefined at data.to");
        if (data["to"] is not JsonArray to) throw new FormatException("Invalid type: Expected Array at data.to");
        for (var i = 0; i < to.Count; i++)
        {
            ev.To.Add(to[i] is JsonValue sv && sv.GetValueKind() == JsonValueKind.String
                ? sv.GetValue<string>()
                : throw new FormatException($"Invalid type: Expected string at data.to.{i}"));
        }
        ev.Subject = ResendWantStr(data, "subject", "data.");
        ResendOptStr(data, "template_id", "data.");
        if (data.ContainsKey("tags"))
        {
            if (data["tags"] is not JsonObject tags) throw new FormatException("Invalid type: Expected Object at data.tags");
            var copy = new JsonObject();
            foreach (var (k, tv) in tags)
            {
                copy[k] = tv is JsonValue x && x.GetValueKind() == JsonValueKind.String
                    ? JsonValue.Create(x.GetValue<string>())
                    : throw new FormatException($"Invalid type: Expected string at data.tags.{k}");
            }
            ev.Tags = copy;
        }
        switch (kind)
        {
            case "email.bounced":
                ev.Detail = ResendSubObject(data, "bounce", ["message", "subType", "type"]);
                ev.BounceMessage = ev.Detail["message"]!.GetValue<string>();
                break;
            case "email.clicked":
                ev.Detail = ResendSubObject(data, "click", ["ipAddress", "link", "timestamp", "userAgent"]);
                break;
            case "email.failed":
                ev.Detail = ResendSubObject(data, "failed", ["reason"]);
                ev.FailedReason = ev.Detail["reason"]!.GetValue<string>();
                break;
        }
        return ev;
    }

    /// <summary>Milliseconds since the Unix epoch for ISO 8601 / RFC 3339 dates. Date-only and offset-less date-times are UTC.</summary>
    public static long? ResendParseIso8601Millis(string s)
    {
        long? Num(int from, int len)
        {
            if (from < 0 || from + len > s.Length) return null;
            long n = 0;
            for (var k = from; k < from + len; k++)
            {
                if (s[k] < '0' || s[k] > '9') return null;
                n = n * 10 + (s[k] - '0');
            }
            return n;
        }
        if (s.Length == 0) return null;
        long year;
        int i;
        if (s[0] is '+' or '-')
        {
            if (Num(1, 6) is not { } y) return null;
            year = s[0] == '-' ? -y : y;
            i = 7;
        }
        else
        {
            if (Num(0, 4) is not { } y) return null;
            year = y;
            i = 4;
        }
        long month = 1, day = 1;
        if (i < s.Length && s[i] == '-')
        {
            if (Num(i + 1, 2) is not { } m) return null;
            month = m;
            i += 3;
            if (i < s.Length && s[i] == '-')
            {
                if (Num(i + 1, 2) is not { } d) return null;
                day = d;
                i += 3;
            }
        }
        if (month is < 1 or > 12 || day is < 1 or > 31) return null;
        long h = 0, mi = 0, sec = 0, ms = 0, offsetMin = 0;
        if (i < s.Length)
        {
            if (s[i] is not ('T' or 't' or ' ')) return null;
            if (Num(i + 1, 2) is not { } hh) return null;
            h = hh;
            if (i + 3 >= s.Length || s[i + 3] != ':') return null;
            if (Num(i + 4, 2) is not { } mm) return null;
            mi = mm;
            i += 6;
            if (i < s.Length && s[i] == ':')
            {
                if (Num(i + 1, 2) is not { } ss) return null;
                sec = ss;
                i += 3;
                if (i < s.Length && s[i] is '.' or ',')
                {
                    var start = i + 1;
                    var end = start;
                    while (end < s.Length && s[end] >= '0' && s[end] <= '9') end++;
                    if (end == start) return null;
                    var frac = s[start..Math.Min(end, start + 3)];
                    ms = long.Parse(frac, CultureInfo.InvariantCulture) * (long)Math.Pow(10, 3 - frac.Length);
                    i = end;
                }
            }
            if (i < s.Length)
            {
                var c = s[i];
                if ((c is 'Z' or 'z') && i + 1 == s.Length) { }
                else if (c is '+' or '-')
                {
                    if (Num(i + 1, 2) is not { } oh) return null;
                    var colon = i + 3 < s.Length && s[i + 3] == ':';
                    if ((colon ? Num(i + 4, 2) : Num(i + 3, 2)) is not { } om) return null;
                    if ((colon ? i + 6 : i + 5) != s.Length) return null;
                    offsetMin = (oh * 60 + om) * (c == '-' ? -1 : 1);
                }
                else return null;
            }
            if (h > 24 || mi > 59 || sec > 59 || (h == 24 && (mi, sec, ms) != (0, 0, 0))) return null;
        }
        // Days from civil (Howard Hinnant).
        var yy = month <= 2 ? year - 1 : year;
        var era = (yy >= 0 ? yy : yy - 399) / 400;
        var yoe = yy - era * 400;
        var mp = (month + 9) % 12;
        var doy = (153 * mp + 2) / 5 + day - 1;
        var doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        var days = era * 146_097 + doe - 719_468;
        return ((days * 24 + h) * 60 + mi - offsetMin) * 60_000 + sec * 1000 + ms;
    }

    static void ResendUpdateWebhookStatus(ResendTx tx, string eventId, ResendWebhookEventStatus status, string? error)
    {
        if (tx.Db.ResendWebhookEvent.EventId.Find(eventId) is not { } row) return;
        tx.Db.ResendWebhookEvent.EventId.Update(row with
        {
            Status = status,
            ErrorMessage = error,
            ProcessedAt = status != ResendWebhookEventStatus.Received ? tx.Timestamp : row.ProcessedAt,
        });
    }

    /// <summary>Apply a stored or incoming payload to resend_email / resend_delivery_event.</summary>
    static (ResendWebhookEventStatus Status, string? Error) ResendApplyEvent(ResendTx tx, string eventId, string payloadJson)
    {
        JsonNode? parsed;
        try { parsed = JsonNode.Parse(payloadJson); }
        catch (Exception) { return (ResendWebhookEventStatus.Failed, "invalid JSON payload"); }
        ResendEmailEvent ev;
        try
        {
            ev = ResendParseEmailEvent(parsed);
        }
        catch (FormatException issue)
        {
            var handled = parsed is JsonObject po && ResendString(po, "type") is { } t && ResendHandledEventTypes.Contains(t);
            return handled ? (ResendWebhookEventStatus.Failed, $"payload validation failed: {issue.Message}") : (ResendWebhookEventStatus.Ignored, null);
        }
        if (ResendParseIso8601Millis(ev.CreatedAt) is not { } millis) return (ResendWebhookEventStatus.Failed, "invalid event timestamp");
        var eventTime = new Timestamp(millis * 1000);
        string? Tag(string k) => ev.Tags is { } t ? ResendString(t, k) : null;
        var a = new ResendEmailUpsert
        {
            ResendId = ev.EmailId,
            FromAddress = ev.From,
            ToAddressesJson = ResendJsJson(ResendStringArray(ev.To)),
            Subject = ev.Subject,
            TagsJson = (ev.Tags is null ? null : ResendJsJson(ev.Tags)),
            UserId = Tag("userId"),
            OrgId = Tag("orgId"),
            StatusUpdatedAt = eventTime,
        };
        switch (ev.Kind)
        {
            case "email.sent": a.Status = ResendEmailStatus.Sent; a.SentAt = eventTime; break;
            case "email.delivered": a.Status = ResendEmailStatus.Delivered; a.DeliveredAt = eventTime; break;
            case "email.delivery_delayed": a.Status = ResendEmailStatus.DeliveryDelayed; break;
            case "email.bounced":
                a.Status = ResendEmailStatus.Bounced;
                a.BouncedAt = eventTime;
                a.BounceJson = ResendJsJson(ev.Detail!);
                a.LastError = ev.BounceMessage;
                break;
            case "email.failed":
                a.Status = ResendEmailStatus.Failed;
                a.FailedAt = eventTime;
                a.FailureReason = ev.FailedReason;
                a.LastError = ev.FailedReason;
                break;
            case "email.complained": a.Complained = true; a.ComplainedAt = eventTime; break;
            case "email.opened": a.Opened = true; a.OpenedAt = eventTime; break;
            case "email.clicked": a.Clicked = true; a.ClickedAt = eventTime; break;
        }
        var now = tx.Timestamp;
        ResendUpsertEmail(tx, now, a);
        if (tx.Db.ResendDeliveryEvent.EventId.Find(eventId) is null)
        {
            tx.Db.ResendDeliveryEvent.Insert(new ResendDeliveryEvent
            {
                EventId = eventId,
                ResendId = ev.EmailId,
                EventType = ev.Kind,
                CreatedAtIso = ev.CreatedAt,
                DetailJson = (ev.Detail is null ? null : ResendJsJson(ev.Detail)),
                InsertedAt = now,
            });
        }
        return (ResendWebhookEventStatus.Processed, null);
    }

    /// <summary>
    /// Verify, store and apply one webhook in the caller's transaction (the reducer and the HTTP route
    /// share this). Returns the stored status, or a rejection when nothing was written.
    /// </summary>
    public static (ResendWebhookEventStatus? Status, ResendWebhookRejection? Rejection) ResendApplyWebhook(
        ResendTx tx, string eventId, string payloadJson, string? signatureHeader, string? timestampHeader)
    {
        (ResendWebhookEventStatus?, ResendWebhookRejection?) Reject(ushort status, string code) => (null, new ResendWebhookRejection(status, code));
        if (eventId.Length == 0 || eventId.Length > ResendMaxWebhookMetadataLength) return Reject(400, ResendErrors.WebhookMetadataInvalid);
        if (payloadJson.Length > ResendMaxWebhookBodyLength) return Reject(413, ResendErrors.WebhookPayloadTooLarge);
        if ((signatureHeader?.Length ?? 0) > ResendMaxWebhookHeaderLength || (timestampHeader?.Length ?? 0) > ResendMaxWebhookHeaderLength)
        {
            return Reject(400, ResendErrors.WebhookHeaderTooLarge);
        }
        if (tx.Db.ResendConfig.Singleton.Find(true)?.WebhookSigningSecret is not { Length: > 0 } secret)
        {
            return Reject(500, ResendErrors.WebhookSecretNotConfigured);
        }
        var verified = Crypto.VerifySvixSignature(new()
        {
            RawBody = payloadJson,
            SvixId = eventId,
            SvixTimestamp = timestampHeader ?? "",
            SvixSignature = signatureHeader ?? "",
            Secret = secret,
            ToleranceSeconds = Crypto.DefaultToleranceSeconds,
            NowSeconds = tx.Timestamp.MicrosecondsSinceUnixEpoch / 1_000_000,
        });
        if (!verified.Ok) return Reject(401, $"{ResendErrors.WebhookSignatureMismatch}:{verified.Reason}");
        if (ResendParseEventType(payloadJson) is not { } eventType || eventType.Length > ResendMaxWebhookMetadataLength)
        {
            return Reject(400, ResendErrors.WebhookPayloadInvalid);
        }
        var existing = tx.Db.ResendWebhookEvent.EventId.Find(eventId);
        if (existing is { Status: ResendWebhookEventStatus.Processed or ResendWebhookEventStatus.Ignored } done) return (done.Status, null);
        if (existing is null)
        {
            tx.Db.ResendWebhookEvent.Insert(new ResendWebhookEvent
            {
                EventId = eventId,
                EventType = eventType,
                PayloadJson = payloadJson,
                SignatureHeader = signatureHeader,
                TimestampHeader = timestampHeader,
                Status = ResendWebhookEventStatus.Received,
                ErrorMessage = null,
                ReceivedAt = tx.Timestamp,
                ProcessedAt = null,
            });
        }
        var (status, error) = ResendApplyEvent(tx, eventId, existing?.PayloadJson ?? payloadJson);
        ResendUpdateWebhookStatus(tx, eventId, status, error);
        return (status, null);
    }

    /// <summary>Ingest a signed webhook (Svix headers passed as arguments). A Failed row commits so it can be read and replayed; rejections write nothing.</summary>
    [SpacetimeDB.Reducer]
    public static void IngestResendWebhook(ReducerContext ctx, string eventId, string eventType, string payloadJson, string? signatureHeader, string? timestampHeader)
    {
        if (ResendParseEventType(payloadJson) != eventType) throw new Exception(ResendErrors.WebhookMetadataMismatch);
        var (_, rejection) = ResendApplyWebhook(new ResendTx(ctx.Db, ctx.Timestamp), eventId, payloadJson, signatureHeader, timestampHeader);
        if (rejection is { } r) throw new Exception(r.Code);
    }

    /// <summary>Admins only: re-apply a stored event.</summary>
    [SpacetimeDB.Reducer]
    public static void ResendReplayWebhookEvent(ReducerContext ctx, string eventId)
    {
        if (!ResendIsAdmin(ctx.Db, ctx.Sender)) throw new Exception(ResendErrors.NotAuthorized);
        var ev = ctx.Db.ResendWebhookEvent.EventId.Find(eventId) ?? throw new Exception($"{ResendErrors.WebhookEventNotFound}:{eventId}");
        var tx = new ResendTx(ctx.Db, ctx.Timestamp);
        var (status, error) = ResendApplyEvent(tx, eventId, ev.PayloadJson);
        ResendUpdateWebhookStatus(tx, eventId, status, error);
    }

    static HttpResponse ResendWebhookJson(ushort status, bool ok, string code) =>
        new(status, HttpVersion.Http11, new List<HttpHeader> { new("content-type", "application/json") },
            HttpBody.FromString(ResendJsJson(new JsonObject { ["ok"] = JsonValue.Create(ok), ["code"] = JsonValue.Create(code) })));

    static string? ResendHeader(HttpRequest req, string name)
    {
        var values = req.Headers
            .Where(h => string.Equals(h.Name, name, StringComparison.OrdinalIgnoreCase))
            .Select(h => Encoding.UTF8.GetString(h.Value))
            .ToList();
        return values.Count == 0 ? null : string.Join(", ", values);
    }

    /// <summary>HTTP route for Resend's webhooks (the TS makeResendWebhookHandler()): register it on your router.</summary>
    [SpacetimeDB.HttpHandler]
    public static HttpResponse ResendWebhook(HandlerContext ctx, HttpRequest req)
    {
        if (!string.Equals(req.Method.Value, "POST", StringComparison.OrdinalIgnoreCase))
        {
            return ResendWebhookJson(405, false, ResendErrors.WebhookMethodNotAllowed);
        }
        var eventId = ResendHeader(req, "svix-id") ?? "";
        var signature = ResendHeader(req, "svix-signature");
        var timestamp = ResendHeader(req, "svix-timestamp");
        var payload = req.Body.ToStringUtf8Lossy();
        var (status, rejection) = ctx.WithTx(tx => ResendApplyWebhook(new ResendTx(tx.Db, tx.Timestamp), eventId, payload, signature, timestamp));
        if (rejection is { } r) return ResendWebhookJson(r.Status, false, r.Code);
        return status == ResendWebhookEventStatus.Failed
            ? ResendWebhookJson(400, false, ResendErrors.WebhookPayloadInvalid)
            : ResendWebhookJson(200, true, "ok");
    }
}
