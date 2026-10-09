// ApiKeys.cs: API keys for server-to-server callers, for C# modules.
//
// Unofficial C# port of @spacetimedb/api-keys (SpacetimeDB PR #5908, commit
// f6ed7dcd1c8f853c97fc7e9f126606fca7adadac, package spacetime-api-keys-ts). Licensed
// Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
// is by Tinker (Gazz-Stripbolt).
//
// Same tables (api_key, api_key_admin_identity, api_key_config, api_key_sweep_tick,
// api_key_usage), procedures (create_api_key, create_api_key_for_subject, rotate_api_key),
// reducers (revoke_api_key, revoke_api_key_for_subject, set_api_keys_config, add_api_keys_admin,
// remove_api_keys_admin, the scheduled api_keys_sweep), views (my_api_keys, api_keys_admin,
// api_key_usage_admin), key format, derivation, hashing, scope rules and error codes as the TS
// package. Given the same operator secret, counter and timestamp, every language derives the
// same key.
//
// Needs the crypto port next to it: add crypto/csharp/Crypto.cs and this file to the project.
//
//     [Reducer(ReducerKind.Init)]
//     public static void Init(ReducerContext ctx) => ApiKeysInstall(ctx);
//
//     // In a procedure that receives a key:
//     var r = ctx.WithTx(tx => ApiKeysVerify(ApiKeysTx.Of(ctx, tx), new VerifyApiKeyArgs(key) { RequiredScope = "files:write" }));
//     if (!r.Allowed) ...
//
// Changes from the TypeScript original:
// - Procedures return Result<T, string>; reducers and helpers throw Exceptions carrying the
//   same api_keys.* codes.
// - api_keys_sweep rejects callers other than the scheduler.
// - Helpers take an ApiKeysTx (database, timestamp, sender, UUID source), built from a
//   ReducerContext or from a procedure's context and transaction.
// - JSON is parsed and re-serialized by a small built-in implementation that follows
//   JSON.parse/JSON.stringify, so normalized scopes_json and metadata_json match TS byte for
//   byte. Nesting deeper than 512 levels is rejected (TS has no limit).
// - ApiKeysRunUsageSweep is public, so hosts (and tests) can run the retention sweep directly.

#pragma warning disable STDB_UNSTABLE
#nullable enable

using System.Globalization;
using System.Text;
using SpacetimeDB;

[SpacetimeDB.Type]
public enum ApiKeyStatus
{
    Active,
    Revoked,
}

[SpacetimeDB.Type]
public partial struct ApiKeySummary
{
    public string KeyId;
    public string Prefix;
    public string OwnerSubject;
    public string Name;
    public string ScopesJson;
    public string? MetadataJson;
    public ApiKeyStatus Status;
    public Timestamp CreatedAt;
    public Timestamp? ExpiresAt;
    public Timestamp? LastUsedAt;
    public Timestamp? RevokedAt;
}

/// <summary>The raw Key is only ever returned here; the database stores its SHA-256.</summary>
[SpacetimeDB.Type]
public partial struct ApiKeyCreateResult
{
    public string KeyId;
    public string Key;
    public string Prefix;
    public string OwnerSubject;
    public string Name;
    public string ScopesJson;
    public string? MetadataJson;
    public ApiKeyStatus Status;
    public Timestamp CreatedAt;
    public Timestamp? ExpiresAt;
}

[SpacetimeDB.Type]
public partial struct ApiKeyUsageSummary
{
    public ulong UsageId;
    public string KeyId;
    public string Prefix;
    public string OwnerSubject;
    public string Action;
    public bool Allowed;
    public string Reason;
    public Timestamp UsedAt;
}

[SpacetimeDB.Type]
public partial struct ApiKeyVerifyResult
{
    public bool Allowed;
    public string Reason;
    public string? KeyId;
    public string? Prefix;
    public string? OwnerSubject;
    public string? ScopesJson;
    public string? MetadataJson;
}

/// <summary>Arguments for ApiKeysCreateInTx.</summary>
public record CreateApiKeyArgs(string OwnerSubject, string Name, string ScopesJson)
{
    public string? MetadataJson { get; init; }
    public uint? ExpiresInSeconds { get; init; }
    public string? KeyPrefix { get; init; }
}

/// <summary>Arguments for ApiKeysVerify.</summary>
public record VerifyApiKeyArgs(string Key)
{
    public string? RequiredScope { get; init; }
    public string? Action { get; init; }
}

/// <summary>What the helpers need from a transaction: works from reducers and from procedures.</summary>
public readonly record struct ApiKeysTx(Local Db, Timestamp Timestamp, Identity Sender, Func<Uuid> NewUuidV7)
{
    public static ApiKeysTx Of(ReducerContext ctx) => new(ctx.Db, ctx.Timestamp, ctx.Sender, ctx.NewUuidV7);
    public static ApiKeysTx Of(ProcedureContext ctx, ProcedureTxContext tx) => new(tx.Db, tx.Timestamp, tx.Sender, ctx.NewUuidV7);
}

public static partial class Module
{
    const string ApiKeysDefaultKeyPrefix = "stdb_live";
    const int ApiKeysMaxNameLength = 120;
    const int ApiKeysMaxOwnerSubjectLength = 256;
    const int ApiKeysMaxScopeLength = 128;
    const int ApiKeysMaxScopes = 128;
    const int ApiKeysMaxMetadataJsonLength = 8192;
    const int ApiKeysMaxRawKeyLength = 128;
    const int ApiKeysMaxActiveKeysPerOwner = 50;
    const uint ApiKeysMaxExpirationSeconds = 60u * 60 * 24 * 365 * 10;
    const int ApiKeysMinSecretLength = 32;
    const uint ApiKeysMinUsageRetentionSeconds = 60 * 60;
    public const uint ApiKeysDefaultUsageRetentionSeconds = 60 * 60 * 24 * 30;
    const int ApiKeysUsageSweepBatch = 1000;
    const long ApiKeysSweepIntervalMicros = 60_000_000;
    /// <summary>Hex characters of the secret stored as the lookup prefix (64 bits).</summary>
    const int ApiKeysLookupSecretChars = 16;

    /// <summary>The api_keys.* codes.</summary>
    public static class ApiKeysErrors
    {
        public const string NotAuthorized = "api_keys.not_authorized";
        public const string CannotRemoveLastAdmin = "api_keys.cannot_remove_last_admin";
        public const string ConfigMissing = "api_keys.config_missing";
        public const string SecretRequired = "api_keys.secret_required";
        public const string InvalidSecret = "api_keys.invalid_secret";
        public const string InvalidUsageRetention = "api_keys.invalid_usage_retention_seconds";
        public const string InvalidKeyPrefix = "api_keys.invalid_key_prefix";
        public const string InvalidName = "api_keys.invalid_name";
        public const string InvalidOwnerSubject = "api_keys.invalid_owner_subject";
        public const string InvalidAction = "api_keys.invalid_action";
        public const string InvalidRequiredScope = "api_keys.invalid_required_scope";
        public const string InvalidScopesJson = "api_keys.invalid_scopes_json";
        public const string InvalidMetadataJson = "api_keys.invalid_metadata_json";
        public const string InvalidExpiresInSeconds = "api_keys.invalid_expires_in_seconds";
        public const string ActiveKeyLimitReached = "api_keys.active_key_limit_reached";
        public const string InvalidKeyId = "api_keys.invalid_key_id";
        public const string NotFound = "api_keys.not_found";
        public const string KeyRevoked = "api_keys.key_revoked";
        public const string KeyExpired = "api_keys.key_expired";
    }

    [SpacetimeDB.Table(Accessor = "ApiKey")]
    public partial struct ApiKey
    {
        [SpacetimeDB.PrimaryKey]
        public string KeyId;
        [SpacetimeDB.Unique]
        public string Prefix;
        public string Hash;
        [SpacetimeDB.Index.BTree]
        public string OwnerSubject;
        public string Name;
        public string ScopesJson;
        public string? MetadataJson;
        [SpacetimeDB.Index.BTree]
        public ApiKeyStatus Status;
        [SpacetimeDB.Index.BTree]
        public Timestamp CreatedAt;
        [SpacetimeDB.Index.BTree]
        public long CreatedAtOrder;
        public Timestamp? ExpiresAt;
        public Timestamp? LastUsedAt;
        public Timestamp? RevokedAt;
    }

    [SpacetimeDB.Table(Accessor = "ApiKeyAdminIdentity")]
    public partial struct ApiKeyAdminIdentity
    {
        [SpacetimeDB.PrimaryKey]
        public Identity Identity;
        public Timestamp AddedAt;
    }

    /// <summary>Created by set_api_keys_config. Counter advances with every derived secret.</summary>
    [SpacetimeDB.Table(Accessor = "ApiKeyConfig")]
    public partial struct ApiKeyConfig
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public string Secret;
        public ulong Counter;
        public uint UsageRetentionSeconds;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "ApiKeySweepTick", Scheduled = nameof(ApiKeysSweep), ScheduledAt = nameof(ApiKeySweepTick.ScheduledAt))]
    public partial struct ApiKeySweepTick
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
    }

    [SpacetimeDB.Table(Accessor = "ApiKeyUsage")]
    public partial struct ApiKeyUsage
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong UsageId;
        [SpacetimeDB.Index.BTree]
        public string KeyId;
        public string Prefix;
        [SpacetimeDB.Index.BTree]
        public string OwnerSubject;
        [SpacetimeDB.Index.BTree]
        public string Action;
        [SpacetimeDB.Index.BTree]
        public bool Allowed;
        [SpacetimeDB.Index.BTree]
        public string Reason;
        [SpacetimeDB.Index.BTree]
        public Timestamp UsedAt;
        [SpacetimeDB.Index.BTree]
        public long UsedAtOrder;
    }

    // -----------------------------------------------------------------------
    // Keys (src/keys.ts)
    // -----------------------------------------------------------------------

    /// <summary>
    /// The module RNG is seeded from the call timestamp, so it can't produce secrets. Key
    /// material is HMAC-SHA256 keyed by the operator's secret over a database-wide counter and
    /// the transaction timestamp.
    /// </summary>
    public static byte[] ApiKeysDeriveSecret(string operatorSecret, ulong counter, long micros) =>
        Crypto.HmacSha256(Encoding.UTF8.GetBytes(operatorSecret), Encoding.UTF8.GetBytes($"spacetimedb-api-keys secret {counter} {micros}"));

    /// <summary>Formats a key as {keyPrefix}_{hex secret}, with its stored lookup prefix.</summary>
    public static (string Key, string Prefix) ApiKeysFormat(string keyPrefix, byte[] secret)
    {
        var hex = Crypto.BytesToHex(secret);
        return ($"{keyPrefix}_{hex}", $"{keyPrefix}_{hex[..ApiKeysLookupSecretChars]}");
    }

    /// <summary>The lookup prefix of a raw key, or null when it can't be one of ours.</summary>
    public static string? ApiKeysExtractLookupPrefix(string key)
    {
        var trimmed = ApiKeysJsTrim(key);
        var last = trimmed.LastIndexOf('_');
        if (last <= 0) return null;
        var secret = trimmed[(last + 1)..];
        if (secret.Length < ApiKeysLookupSecretChars) return null;
        return $"{trimmed[..last]}_{secret[..ApiKeysLookupSecretChars]}";
    }

    public static string ApiKeysHash(string key) => Crypto.BytesToHex(Crypto.Sha256(Encoding.UTF8.GetBytes(key)));

    public static bool ApiKeysMatchesHash(string key, string expectedHex)
    {
        try
        {
            return Crypto.TimingSafeEqual(Crypto.HexToBytes(expectedHex), Crypto.Sha256(Encoding.UTF8.GetBytes(key)));
        }
        catch
        {
            return false;
        }
    }

    /// <summary>Whether scopesJson (a JSON array of scope strings) grants requiredScope. "*" grants everything, "files:*" every files: scope.</summary>
    public static bool ApiKeysHasScope(string scopesJson, string? requiredScope)
    {
        if (requiredScope is null || ApiKeysJsTrim(requiredScope).Length == 0) return true;
        var required = ApiKeysJsTrim(requiredScope);
        if (ApiKeysJson.Parse(scopesJson) is not List<object?> scopes) return false;
        return scopes.Any(s => s is string scope
            && (scope == "*" || scope == required || (scope.EndsWith(":*", StringComparison.Ordinal) && required.StartsWith(scope[..^1], StringComparison.Ordinal))));
    }

    // -----------------------------------------------------------------------
    // Operations (src/submodule/operations.ts)
    // -----------------------------------------------------------------------

    /// <summary>JavaScript's \s (also what String.prototype.trim removes).</summary>
    static bool ApiKeysIsJsWhitespace(char c) => c switch
    {
        >= '\u0009' and <= '\u000d' => true,
        ' ' or '\u00a0' or '\u1680' or '\u2028' or '\u2029' or '\u202f' or '\u205f' or '\u3000' or '\ufeff' => true,
        >= '\u2000' and <= '\u200a' => true,
        _ => false,
    };

    static string ApiKeysJsTrim(string s)
    {
        int a = 0, b = s.Length;
        while (a < b && ApiKeysIsJsWhitespace(s[a])) a++;
        while (b > a && ApiKeysIsJsWhitespace(s[b - 1])) b--;
        return s[a..b];
    }

    static bool ApiKeysIsScopeChar(char c) => char.IsAsciiLetterOrDigit(c) || c is ':' or '_' or '*' or '.' or '-';

    /// <summary>/^[A-Za-z][A-Za-z0-9_]{1,31}$/</summary>
    static string ApiKeysNormalizeKeyPrefix(string? prefix)
    {
        var v = ApiKeysJsTrim(prefix ?? ApiKeysDefaultKeyPrefix);
        var ok = v.Length is >= 2 and <= 32 && char.IsAsciiLetter(v[0]) && v.Skip(1).All(c => char.IsAsciiLetterOrDigit(c) || c == '_');
        if (!ok) throw new Exception(ApiKeysErrors.InvalidKeyPrefix);
        return v;
    }

    /// <summary>Trimmed, with runs of whitespace collapsed to one space.</summary>
    static string ApiKeysNormalizeName(string name)
    {
        var sb = new StringBuilder();
        var pendingSpace = false;
        foreach (var c in ApiKeysJsTrim(name))
        {
            if (ApiKeysIsJsWhitespace(c))
            {
                pendingSpace = true;
                continue;
            }
            if (pendingSpace) sb.Append(' ');
            pendingSpace = false;
            sb.Append(c);
        }
        var v = sb.ToString();
        if (v.Length == 0 || v.Length > ApiKeysMaxNameLength) throw new Exception(ApiKeysErrors.InvalidName);
        return v;
    }

    static string ApiKeysNormalizeOwnerSubject(string ownerSubject)
    {
        var v = ApiKeysJsTrim(ownerSubject);
        if (v.Length == 0 || v.Length > ApiKeysMaxOwnerSubjectLength) throw new Exception(ApiKeysErrors.InvalidOwnerSubject);
        return v;
    }

    static string ApiKeysNormalizeAction(string? action, string? requiredScope)
    {
        var v = ApiKeysJsTrim(action ?? requiredScope ?? "verify");
        if (v.Length == 0 || v.Length > ApiKeysMaxScopeLength) throw new Exception(ApiKeysErrors.InvalidAction);
        return v;
    }

    static string? ApiKeysNormalizeRequiredScope(string? requiredScope)
    {
        if (requiredScope is null) return null;
        var v = ApiKeysJsTrim(requiredScope);
        if (v.Length == 0 || v.Length > ApiKeysMaxScopeLength || !v.All(ApiKeysIsScopeChar)) throw new Exception(ApiKeysErrors.InvalidRequiredScope);
        return v;
    }

    static string ApiKeysNormalizeScopesJson(string scopesJson)
    {
        if (ApiKeysJson.Parse(scopesJson) is not List<object?> items || items.Count == 0 || items.Count > ApiKeysMaxScopes)
        {
            throw new Exception(ApiKeysErrors.InvalidScopesJson);
        }
        var scopes = new List<string>();
        foreach (var item in items)
        {
            if (item is not string raw) throw new Exception(ApiKeysErrors.InvalidScopesJson);
            var scope = ApiKeysJsTrim(raw);
            if (scope.Length == 0 || scope.Length > ApiKeysMaxScopeLength || !scope.All(ApiKeysIsScopeChar)) throw new Exception(ApiKeysErrors.InvalidScopesJson);
            if (!scopes.Contains(scope)) scopes.Add(scope);
        }
        return ApiKeysJson.Stringify(scopes.Cast<object?>().ToList());
    }

    static string? ApiKeysNormalizeMetadataJson(string? metadataJson)
    {
        if (metadataJson is null) return null;
        var v = ApiKeysJsTrim(metadataJson);
        if (v.Length == 0) return null;
        if (v.Length > ApiKeysMaxMetadataJsonLength) throw new Exception(ApiKeysErrors.InvalidMetadataJson);
        if (ApiKeysJson.Parse(v) is not ApiKeysJson.Obj parsed) throw new Exception(ApiKeysErrors.InvalidMetadataJson);
        return ApiKeysJson.Stringify(parsed);
    }

    static Timestamp? ApiKeysExpiresAt(Timestamp now, uint? expiresInSeconds)
    {
        if (expiresInSeconds is not { } s) return null;
        if (s == 0 || s > ApiKeysMaxExpirationSeconds) throw new Exception(ApiKeysErrors.InvalidExpiresInSeconds);
        return new Timestamp(now.MicrosecondsSinceUnixEpoch + (long)s * 1_000_000);
    }

    static bool ApiKeysIsExpired(ApiKey row, Timestamp now) =>
        row.ExpiresAt is { } e && e.MicrosecondsSinceUnixEpoch <= now.MicrosecondsSinceUnixEpoch;

    /// <summary>The TS toHexString() of an identity: lowercase hex, no 0x.</summary>
    public static string ApiKeysIdentityHex(Identity identity)
    {
        var s = identity.ToString();
        if (s.StartsWith("0x", StringComparison.OrdinalIgnoreCase)) s = s[2..];
        return s.ToLowerInvariant();
    }

    static ApiKeySummary ApiKeysToSummary(ApiKey r) => new()
    {
        KeyId = r.KeyId,
        Prefix = r.Prefix,
        OwnerSubject = r.OwnerSubject,
        Name = r.Name,
        ScopesJson = r.ScopesJson,
        MetadataJson = r.MetadataJson,
        Status = r.Status,
        CreatedAt = r.CreatedAt,
        ExpiresAt = r.ExpiresAt,
        LastUsedAt = r.LastUsedAt,
        RevokedAt = r.RevokedAt,
    };

    static ApiKeyCreateResult ApiKeysToCreateResult(ApiKey r, string key) => new()
    {
        KeyId = r.KeyId,
        Key = key,
        Prefix = r.Prefix,
        OwnerSubject = r.OwnerSubject,
        Name = r.Name,
        ScopesJson = r.ScopesJson,
        MetadataJson = r.MetadataJson,
        Status = r.Status,
        CreatedAt = r.CreatedAt,
        ExpiresAt = r.ExpiresAt,
    };

    static (string Key, string Prefix) ApiKeysGenerateRawKey(ApiKeysTx tx, string keyPrefix)
    {
        var cfg = tx.Db.ApiKeyConfig.Singleton.Find(true) ?? throw new Exception(ApiKeysErrors.ConfigMissing);
        var counter = cfg.Counter + 1;
        var secret = ApiKeysDeriveSecret(cfg.Secret, counter, tx.Timestamp.MicrosecondsSinceUnixEpoch);
        tx.Db.ApiKeyConfig.Singleton.Update(cfg with { Counter = counter });
        return ApiKeysFormat(keyPrefix, secret);
    }

    static void ApiKeysRecordUsage(ApiKeysTx tx, string keyId, string prefix, string ownerSubject, string action, bool allowed, string reason) =>
        tx.Db.ApiKeyUsage.Insert(new ApiKeyUsage
        {
            UsageId = 0,
            KeyId = keyId,
            Prefix = prefix,
            OwnerSubject = ownerSubject,
            Action = action,
            Allowed = allowed,
            Reason = reason,
            UsedAt = tx.Timestamp,
            UsedAtOrder = -tx.Timestamp.MicrosecondsSinceUnixEpoch,
        });

    public static bool ApiKeysIsAdmin(Local db, Identity identity) => db.ApiKeyAdminIdentity.Identity.Find(identity) is not null;

    /// <summary>Throws api_keys.not_authorized unless the caller is an API key admin.</summary>
    public static void ApiKeysRequireAdmin(Local db, Identity sender)
    {
        if (!ApiKeysIsAdmin(db, sender)) throw new Exception(ApiKeysErrors.NotAuthorized);
    }

    /// <summary>Create a key in the caller's transaction. At most 50 active keys per owner.</summary>
    public static ApiKeyCreateResult ApiKeysCreateInTx(ApiKeysTx tx, CreateApiKeyArgs args)
    {
        var ownerSubject = ApiKeysNormalizeOwnerSubject(args.OwnerSubject);
        var name = ApiKeysNormalizeName(args.Name);
        var scopesJson = ApiKeysNormalizeScopesJson(args.ScopesJson);
        var metadataJson = ApiKeysNormalizeMetadataJson(args.MetadataJson);
        var expiresAt = ApiKeysExpiresAt(tx.Timestamp, args.ExpiresInSeconds);
        var keyPrefix = ApiKeysNormalizeKeyPrefix(args.KeyPrefix);
        var active = tx.Db.ApiKey.OwnerSubject.Filter(ownerSubject).Count(r => r.Status == ApiKeyStatus.Active && !ApiKeysIsExpired(r, tx.Timestamp));
        if (active >= ApiKeysMaxActiveKeysPerOwner) throw new Exception(ApiKeysErrors.ActiveKeyLimitReached);
        var (key, prefix) = ApiKeysGenerateRawKey(tx, keyPrefix);
        var row = tx.Db.ApiKey.Insert(new ApiKey
        {
            KeyId = $"ak_{tx.NewUuidV7()}",
            Prefix = prefix,
            Hash = ApiKeysHash(key),
            OwnerSubject = ownerSubject,
            Name = name,
            ScopesJson = scopesJson,
            MetadataJson = metadataJson,
            Status = ApiKeyStatus.Active,
            CreatedAt = tx.Timestamp,
            CreatedAtOrder = -tx.Timestamp.MicrosecondsSinceUnixEpoch,
            ExpiresAt = expiresAt,
            LastUsedAt = null,
            RevokedAt = null,
        });
        ApiKeysRecordUsage(tx, row.KeyId, row.Prefix, row.OwnerSubject, "create", true, "created");
        return ApiKeysToCreateResult(row, key);
    }

    static ApiKeyVerifyResult ApiKeysDenied(string? prefix, string reason) =>
        new() { Allowed = false, Reason = reason, KeyId = null, Prefix = prefix, OwnerSubject = null, ScopesJson = null, MetadataJson = null };

    /// <summary>
    /// Check a raw key (and optionally a scope) in the caller's transaction. Checks against a
    /// known key are recorded in api_key_usage; malformed and unknown keys aren't. Throws only
    /// for an invalid RequiredScope or Action.
    /// </summary>
    public static ApiKeyVerifyResult ApiKeysVerify(ApiKeysTx tx, VerifyApiKeyArgs args)
    {
        var requiredScope = ApiKeysNormalizeRequiredScope(args.RequiredScope);
        var action = ApiKeysNormalizeAction(args.Action, requiredScope);
        var key = ApiKeysJsTrim(args.Key);
        if (key.Length == 0 || key.Length > ApiKeysMaxRawKeyLength) return ApiKeysDenied(null, "invalid_key");
        if (ApiKeysExtractLookupPrefix(key) is not { } prefix) return ApiKeysDenied(null, "invalid_key");
        if (tx.Db.ApiKey.Prefix.Find(prefix) is not { } row) return ApiKeysDenied(prefix, "unknown_key");
        ApiKeyVerifyResult Deny(string reason)
        {
            ApiKeysRecordUsage(tx, row.KeyId, row.Prefix, row.OwnerSubject, action, false, reason);
            return ApiKeysDenied(row.Prefix, reason);
        }
        if (!ApiKeysMatchesHash(key, row.Hash)) return Deny("invalid_key");
        if (row.Status != ApiKeyStatus.Active) return Deny("revoked");
        if (ApiKeysIsExpired(row, tx.Timestamp)) return Deny("expired");
        if (!ApiKeysHasScope(row.ScopesJson, requiredScope)) return Deny("scope_denied");
        tx.Db.ApiKey.KeyId.Update(row with { LastUsedAt = tx.Timestamp });
        ApiKeysRecordUsage(tx, row.KeyId, row.Prefix, row.OwnerSubject, action, true, "allowed");
        return new ApiKeyVerifyResult
        {
            Allowed = true,
            Reason = "allowed",
            KeyId = row.KeyId,
            Prefix = row.Prefix,
            OwnerSubject = row.OwnerSubject,
            ScopesJson = row.ScopesJson,
            MetadataJson = row.MetadataJson,
        };
    }

    static ApiKey ApiKeysFindManaged(ApiKeysTx tx, string keyId, string? ownerSubject)
    {
        var id = ApiKeysJsTrim(keyId);
        if (id.Length == 0) throw new Exception(ApiKeysErrors.InvalidKeyId);
        var row = tx.Db.ApiKey.KeyId.Find(id) ?? throw new Exception(ApiKeysErrors.NotFound);
        var subject = ApiKeysNormalizeOwnerSubject(ownerSubject ?? ApiKeysIdentityHex(tx.Sender));
        if (row.OwnerSubject != subject && !ApiKeysIsAdmin(tx.Db, tx.Sender)) throw new Exception(ApiKeysErrors.NotAuthorized);
        return row;
    }

    /// <summary>Revoke a key. Owners (by subject) and admins may. Revoking twice is a no-op.</summary>
    public static void ApiKeysRevokeInTx(ApiKeysTx tx, string keyId, string? ownerSubject)
    {
        var row = ApiKeysFindManaged(tx, keyId, ownerSubject);
        if (row.Status == ApiKeyStatus.Revoked) return;
        row = tx.Db.ApiKey.KeyId.Update(row with { Status = ApiKeyStatus.Revoked, RevokedAt = tx.Timestamp });
        ApiKeysRecordUsage(tx, row.KeyId, row.Prefix, row.OwnerSubject, "revoke", true, "revoked");
    }

    /// <summary>Replace a live key's secret (same key id). Keeps the expiry unless a new one is given.</summary>
    public static ApiKeyCreateResult ApiKeysRotateInTx(ApiKeysTx tx, string keyId, string? ownerSubject, uint? expiresInSeconds, string? keyPrefix)
    {
        var row = ApiKeysFindManaged(tx, keyId, ownerSubject);
        // Only live keys rotate, so rotation never revives a key or exceeds the cap.
        if (row.Status != ApiKeyStatus.Active) throw new Exception(ApiKeysErrors.KeyRevoked);
        if (ApiKeysIsExpired(row, tx.Timestamp)) throw new Exception(ApiKeysErrors.KeyExpired);
        var prefixArg = ApiKeysNormalizeKeyPrefix(keyPrefix);
        var expiresAt = expiresInSeconds is null ? row.ExpiresAt : ApiKeysExpiresAt(tx.Timestamp, expiresInSeconds);
        var (key, prefix) = ApiKeysGenerateRawKey(tx, prefixArg);
        var updated = tx.Db.ApiKey.KeyId.Update(row with { Prefix = prefix, Hash = ApiKeysHash(key), ExpiresAt = expiresAt, LastUsedAt = null });
        ApiKeysRecordUsage(tx, updated.KeyId, prefix, updated.OwnerSubject, "rotate", true, "rotated");
        return ApiKeysToCreateResult(updated, key);
    }

    /// <summary>Delete up to 1,000 usage rows older than the retention window. Returns the count.</summary>
    public static uint ApiKeysRunUsageSweep(Local db, Timestamp now)
    {
        var retention = db.ApiKeyConfig.Singleton.Find(true)?.UsageRetentionSeconds ?? ApiKeysDefaultUsageRetentionSeconds;
        var cutoff = new Timestamp(now.MicrosecondsSinceUnixEpoch - (long)retention * 1_000_000);
        var expired = db.ApiKeyUsage.UsedAt.Filter(new Bound<Timestamp>(new Timestamp(long.MinValue), cutoff))
            .Take(ApiKeysUsageSweepBatch)
            .Select(r => r.UsageId)
            .ToList();
        foreach (var id in expired) db.ApiKeyUsage.UsageId.Delete(id);
        return (uint)expired.Count;
    }

    // -----------------------------------------------------------------------
    // Install, client operations
    // -----------------------------------------------------------------------

    /// <summary>Call from your Init reducer: the caller becomes an API key admin and the usage sweep runs every 60 s.</summary>
    public static void ApiKeysInstall(ReducerContext ctx)
    {
        if (ctx.Db.ApiKeyAdminIdentity.Identity.Find(ctx.Sender) is null)
        {
            ctx.Db.ApiKeyAdminIdentity.Insert(new ApiKeyAdminIdentity { Identity = ctx.Sender, AddedAt = ctx.Timestamp });
        }
        if (ctx.Db.ApiKeySweepTick.Count == 0)
        {
            ctx.Db.ApiKeySweepTick.Insert(new ApiKeySweepTick { ScheduledId = 0, ScheduledAt = new ScheduleAt.Interval(new TimeDuration(ApiKeysSweepIntervalMicros)) });
        }
    }

    static Result<T, string> ApiKeysTry<T>(Func<T> f)
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

    /// <summary>Create a key owned by the caller.</summary>
    [SpacetimeDB.Procedure]
    public static Result<ApiKeyCreateResult, string> CreateApiKey(ProcedureContext ctx, string name, string scopesJson, string? metadataJson, uint? expiresInSeconds, string? keyPrefix) =>
        ApiKeysTry(() => ctx.WithTx(tx => ApiKeysCreateInTx(ApiKeysTx.Of(ctx, tx), new CreateApiKeyArgs(ApiKeysIdentityHex(ctx.Sender), name, scopesJson)
        {
            MetadataJson = metadataJson,
            ExpiresInSeconds = expiresInSeconds,
            KeyPrefix = keyPrefix,
        })));

    /// <summary>Create a key for any subject (admins).</summary>
    [SpacetimeDB.Procedure]
    public static Result<ApiKeyCreateResult, string> CreateApiKeyForSubject(ProcedureContext ctx, string ownerSubject, string name, string scopesJson, string? metadataJson, uint? expiresInSeconds, string? keyPrefix) =>
        ApiKeysTry(() => ctx.WithTx(tx =>
        {
            ApiKeysRequireAdmin(tx.Db, tx.Sender);
            return ApiKeysCreateInTx(ApiKeysTx.Of(ctx, tx), new CreateApiKeyArgs(ownerSubject, name, scopesJson)
            {
                MetadataJson = metadataJson,
                ExpiresInSeconds = expiresInSeconds,
                KeyPrefix = keyPrefix,
            });
        }));

    [SpacetimeDB.Procedure]
    public static Result<ApiKeyCreateResult, string> RotateApiKey(ProcedureContext ctx, string keyId, uint? expiresInSeconds, string? keyPrefix) =>
        ApiKeysTry(() => ctx.WithTx(tx => ApiKeysRotateInTx(ApiKeysTx.Of(ctx, tx), keyId, ApiKeysIdentityHex(ctx.Sender), expiresInSeconds, keyPrefix)));

    [SpacetimeDB.Reducer]
    public static void RevokeApiKey(ReducerContext ctx, string keyId) =>
        ApiKeysRevokeInTx(ApiKeysTx.Of(ctx), keyId, ApiKeysIdentityHex(ctx.Sender));

    [SpacetimeDB.Reducer]
    public static void RevokeApiKeyForSubject(ReducerContext ctx, string keyId, string ownerSubject)
    {
        ApiKeysRequireAdmin(ctx.Db, ctx.Sender);
        ApiKeysRevokeInTx(ApiKeysTx.Of(ctx), keyId, ownerSubject);
    }

    /// <summary>Admins. secret (32+ characters, generated outside the module) is required on the first call.</summary>
    [SpacetimeDB.Reducer]
    public static void SetApiKeysConfig(ReducerContext ctx, string? secret, uint? usageRetentionSeconds)
    {
        ApiKeysRequireAdmin(ctx.Db, ctx.Sender);
        var existing = ctx.Db.ApiKeyConfig.Singleton.Find(true);
        var s = secret ?? existing?.Secret ?? throw new Exception(ApiKeysErrors.SecretRequired);
        if (s.Length < ApiKeysMinSecretLength) throw new Exception(ApiKeysErrors.InvalidSecret);
        var retention = usageRetentionSeconds ?? existing?.UsageRetentionSeconds ?? ApiKeysDefaultUsageRetentionSeconds;
        if (retention < ApiKeysMinUsageRetentionSeconds || retention > ApiKeysMaxExpirationSeconds) throw new Exception(ApiKeysErrors.InvalidUsageRetention);
        var row = new ApiKeyConfig { Singleton = true, Secret = s, Counter = existing?.Counter ?? 0, UsageRetentionSeconds = retention, UpdatedAt = ctx.Timestamp };
        if (existing is not null) ctx.Db.ApiKeyConfig.Singleton.Update(row);
        else ctx.Db.ApiKeyConfig.Insert(row);
    }

    [SpacetimeDB.Reducer]
    public static void AddApiKeysAdmin(ReducerContext ctx, Identity identity)
    {
        ApiKeysRequireAdmin(ctx.Db, ctx.Sender);
        if (ctx.Db.ApiKeyAdminIdentity.Identity.Find(identity) is null)
        {
            ctx.Db.ApiKeyAdminIdentity.Insert(new ApiKeyAdminIdentity { Identity = identity, AddedAt = ctx.Timestamp });
        }
    }

    [SpacetimeDB.Reducer]
    public static void RemoveApiKeysAdmin(ReducerContext ctx, Identity identity)
    {
        ApiKeysRequireAdmin(ctx.Db, ctx.Sender);
        if (ctx.Db.ApiKeyAdminIdentity.Identity.Find(identity) is null) return;
        if (ctx.Db.ApiKeyAdminIdentity.Count <= 1) throw new Exception(ApiKeysErrors.CannotRemoveLastAdmin);
        ctx.Db.ApiKeyAdminIdentity.Identity.Delete(identity);
    }

    /// <summary>Deletes a bounded batch of usage rows older than the retention window. Every 60 s.</summary>
    [SpacetimeDB.Reducer]
    public static void ApiKeysSweep(ReducerContext ctx, ApiKeySweepTick tick)
    {
        if (ctx.Sender != ctx.DatabaseIdentity) throw new Exception(ApiKeysErrors.NotAuthorized);
        ApiKeysRunUsageSweep(ctx.Db, ctx.Timestamp);
    }

    /// <summary>Up to 500 of the caller's keys (never the raw key or its hash).</summary>
    [SpacetimeDB.View(Accessor = "MyApiKeys", Public = true)]
    public static List<ApiKeySummary> MyApiKeys(ViewContext ctx) =>
        ctx.Db.ApiKey.OwnerSubject.Filter(ApiKeysIdentityHex(ctx.Sender)).Take(500).Select(ApiKeysToSummary).ToList();

    /// <summary>The 200 newest keys, for admins (others get nothing).</summary>
    [SpacetimeDB.View(Accessor = "ApiKeysAdmin", Public = true)]
    public static List<ApiKeySummary> ApiKeysAdmin(ViewContext ctx)
    {
        if (ctx.Db.ApiKeyAdminIdentity.Identity.Find(ctx.Sender) is null) return [];
        return ctx.Db.ApiKey.CreatedAtOrder.Filter(new Bound<long>(long.MinValue, long.MaxValue)).Take(200).Select(ApiKeysToSummary).ToList();
    }

    /// <summary>The 100 newest usage records, for admins (others get nothing).</summary>
    [SpacetimeDB.View(Accessor = "ApiKeyUsageAdmin", Public = true)]
    public static List<ApiKeyUsageSummary> ApiKeyUsageAdmin(ViewContext ctx)
    {
        if (ctx.Db.ApiKeyAdminIdentity.Identity.Find(ctx.Sender) is null) return [];
        return ctx.Db.ApiKeyUsage.UsedAtOrder.Filter(new Bound<long>(long.MinValue, long.MaxValue)).Take(100).Select(r => new ApiKeyUsageSummary
        {
            UsageId = r.UsageId,
            KeyId = r.KeyId,
            Prefix = r.Prefix,
            OwnerSubject = r.OwnerSubject,
            Action = r.Action,
            Allowed = r.Allowed,
            Reason = r.Reason,
            UsedAt = r.UsedAt,
        }).ToList();
    }
}

/// <summary>
/// Just enough of JSON.parse / JSON.stringify to normalize like TS. Values are null, bool,
/// double, string, List&lt;object?&gt; (arrays) and Obj (objects, in JavaScript property order:
/// integer-like keys ascending, then insertion order).
/// </summary>
public static class ApiKeysJson
{
    const int MaxDepth = 512;

    public sealed class Obj : List<KeyValuePair<string, object?>> { }

    /// <summary>JSON.parse: strict JSON, whitespace is space, tab, CR and LF only. Returns Invalid if the text isn't JSON.</summary>
    public static object? Parse(string text)
    {
        var p = new Parser(text);
        try
        {
            p.Ws();
            var v = p.Value(0);
            p.Ws();
            return p.AtEnd ? v : Invalid;
        }
        catch (FormatException)
        {
            return Invalid;
        }
    }

    /// <summary>Returned by Parse for invalid input (distinct from JSON null).</summary>
    public static readonly object Invalid = new();

    sealed class Parser(string s)
    {
        int i;
        public bool AtEnd => i == s.Length;
        static FormatException Bad() => new();

        public void Ws()
        {
            while (i < s.Length && s[i] is ' ' or '\t' or '\n' or '\r') i++;
        }

        bool Eat(char c)
        {
            if (i < s.Length && s[i] == c)
            {
                i++;
                return true;
            }
            return false;
        }

        object? Lit(string word, object? v)
        {
            if (string.CompareOrdinal(s, i, word, 0, word.Length) != 0) throw Bad();
            i += word.Length;
            return v;
        }

        public object? Value(int depth)
        {
            if (depth > MaxDepth || i >= s.Length) throw Bad();
            switch (s[i])
            {
                case '{':
                {
                    i++;
                    var fields = new List<KeyValuePair<string, object?>>();
                    Ws();
                    if (!Eat('}'))
                    {
                        while (true)
                        {
                            Ws();
                            if (i >= s.Length || s[i] != '"') throw Bad();
                            var k = Str();
                            Ws();
                            if (!Eat(':')) throw Bad();
                            Ws();
                            var v = Value(depth + 1);
                            var at = fields.FindIndex(f => f.Key == k);
                            if (at >= 0) fields[at] = new(k, v);
                            else fields.Add(new(k, v));
                            Ws();
                            if (Eat(',')) continue;
                            if (Eat('}')) break;
                            throw Bad();
                        }
                    }
                    // Integer-like keys first, ascending; OrderBy is stable, so the rest keep their order.
                    var obj = new Obj();
                    obj.AddRange(fields.OrderBy(f => ArrayIndex(f.Key) is { } n ? (0, n) : (1, 0L)));
                    return obj;
                }
                case '[':
                {
                    i++;
                    var items = new List<object?>();
                    Ws();
                    if (!Eat(']'))
                    {
                        while (true)
                        {
                            Ws();
                            items.Add(Value(depth + 1));
                            Ws();
                            if (Eat(',')) continue;
                            if (Eat(']')) break;
                            throw Bad();
                        }
                    }
                    return items;
                }
                case '"':
                    return Str();
                case 't':
                    return Lit("true", true);
                case 'f':
                    return Lit("false", false);
                case 'n':
                    return Lit("null", null);
                default:
                    return Number();
            }
        }

        int Digits()
        {
            var st = i;
            while (i < s.Length && s[i] is >= '0' and <= '9') i++;
            return i - st;
        }

        object Number()
        {
            var start = i;
            Eat('-');
            if (Eat('0'))
            {
                if (i < s.Length && s[i] is >= '0' and <= '9') throw Bad();
            }
            else if (Digits() == 0) throw Bad();
            if (Eat('.') && Digits() == 0) throw Bad();
            if (Eat('e') || Eat('E'))
            {
                if (!Eat('+')) Eat('-');
                if (Digits() == 0) throw Bad();
            }
            return double.Parse(s.AsSpan(start, i - start), NumberStyles.Float, CultureInfo.InvariantCulture);
        }

        string Str()
        {
            i++; // opening quote
            var sb = new StringBuilder();
            while (true)
            {
                if (i >= s.Length) throw Bad();
                var c = s[i++];
                switch (c)
                {
                    case '"':
                        return sb.ToString();
                    case '\\':
                        if (i >= s.Length) throw Bad();
                        var e = s[i++];
                        switch (e)
                        {
                            case '"': sb.Append('"'); break;
                            case '\\': sb.Append('\\'); break;
                            case '/': sb.Append('/'); break;
                            case 'b': sb.Append('\b'); break;
                            case 'f': sb.Append('\f'); break;
                            case 'n': sb.Append('\n'); break;
                            case 'r': sb.Append('\r'); break;
                            case 't': sb.Append('\t'); break;
                            case 'u':
                                if (i + 4 > s.Length || !s.AsSpan(i, 4).ToString().All(char.IsAsciiHexDigit)) throw Bad();
                                sb.Append((char)ushort.Parse(s.AsSpan(i, 4), NumberStyles.HexNumber, CultureInfo.InvariantCulture));
                                i += 4;
                                break;
                            default:
                                throw Bad();
                        }
                        break;
                    default:
                        if (c < 0x20) throw Bad();
                        sb.Append(c);
                        break;
                }
            }
        }
    }

    /// <summary>A canonical array index ("0" .. "4294967294"), as JavaScript orders them.</summary>
    static long? ArrayIndex(string k)
    {
        if (k.Length == 0 || k.Length > 10 || (k.Length > 1 && k[0] == '0') || !k.All(c => c is >= '0' and <= '9')) return null;
        var n = long.Parse(k, CultureInfo.InvariantCulture);
        return n < uint.MaxValue ? n : null;
    }

    /// <summary>JSON.stringify (compact).</summary>
    public static string Stringify(object? v)
    {
        var sb = new StringBuilder();
        Write(v, sb);
        return sb.ToString();
    }

    static void Write(object? v, StringBuilder sb)
    {
        switch (v)
        {
            case null:
                sb.Append("null");
                break;
            case bool b:
                sb.Append(b ? "true" : "false");
                break;
            case double d:
                sb.Append(Number(d));
                break;
            case string s:
                Quote(s, sb);
                break;
            case Obj o:
                sb.Append('{');
                for (var k = 0; k < o.Count; k++)
                {
                    if (k > 0) sb.Append(',');
                    Quote(o[k].Key, sb);
                    sb.Append(':');
                    Write(o[k].Value, sb);
                }
                sb.Append('}');
                break;
            case List<object?> a:
                sb.Append('[');
                for (var k = 0; k < a.Count; k++)
                {
                    if (k > 0) sb.Append(',');
                    Write(a[k], sb);
                }
                sb.Append(']');
                break;
            default:
                throw new ArgumentException("not a JSON value");
        }
    }

    /// <summary>JSON's QuoteJSONString: short escapes, \u00xx for other controls and lone surrogates.</summary>
    static void Quote(string s, StringBuilder sb)
    {
        sb.Append('"');
        for (var k = 0; k < s.Length; k++)
        {
            var c = s[k];
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
                    else if (char.IsHighSurrogate(c) && k + 1 < s.Length && char.IsLowSurrogate(s[k + 1]))
                    {
                        sb.Append(c).Append(s[k + 1]);
                        k++;
                    }
                    else if (char.IsSurrogate(c)) sb.Append($"\\u{(int)c:x4}");
                    else sb.Append(c);
                    break;
            }
        }
        sb.Append('"');
    }

    /// <summary>Number.prototype.toString() for finite numbers; "null" otherwise (as JSON.stringify).</summary>
    public static string Number(double x)
    {
        if (!double.IsFinite(x)) return "null";
        if (x == 0) return "0";
        // Shortest round-trip digits and the decimal exponent, then ECMAScript's layout.
        var r = Math.Abs(x).ToString("R", CultureInfo.InvariantCulture);
        var exp = 0;
        var ePos = r.IndexOfAny(['E', 'e']);
        if (ePos >= 0)
        {
            exp = int.Parse(r[(ePos + 1)..], NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture);
            r = r[..ePos];
        }
        var dot = r.IndexOf('.');
        var digits = dot >= 0 ? r.Remove(dot, 1) : r;
        var n = (dot >= 0 ? dot : r.Length) + exp;
        var lead = 0;
        while (lead < digits.Length - 1 && digits[lead] == '0') lead++;
        digits = digits[lead..];
        n -= lead;
        digits = digits.TrimEnd('0');
        if (digits.Length == 0) digits = "0";
        var k = digits.Length;
        var sb = new StringBuilder();
        if (x < 0) sb.Append('-');
        if (k <= n && n <= 21) sb.Append(digits).Append('0', n - k);
        else if (0 < n && n <= 21) sb.Append(digits, 0, n).Append('.').Append(digits, n, k - n);
        else if (-6 < n && n <= 0) sb.Append("0.").Append('0', -n).Append(digits);
        else
        {
            sb.Append(digits[0]);
            if (k > 1) sb.Append('.').Append(digits, 1, k - 1);
            sb.Append('e').Append(n - 1 >= 0 ? '+' : '-').Append(Math.Abs(n - 1));
        }
        return sb.ToString();
    }
}
