// Auth.cs: accounts, sign-in and sessions for C# modules.
//
// Unofficial C# port of @spacetimedb/auth (SpacetimeDB PR #5905, commit
// 37ee2ad627568f95df8229647cff5a482d629c25, package spacetime-auth-ts). Licensed Apache-2.0,
// like the original. Original design and behaviour by Clockwork Labs; this port is by Tinker
// (Gazz-Stripbolt).
//
// Same tables, error codes, password hashes (scrypt$N$r$p$salt$hash), ES256 session JWTs, PEM
// encodings, key ids, derived secrets and HTTP behaviour as the TS package: given the same
// signing key, counter and timestamp, every language mints the same tokens.
//
// Needs the crypto and rate-limit ports next to it: add crypto/csharp/Crypto.cs,
// rate-limit/csharp/RateLimit.cs and this file to the project.
//
//     [Reducer(ReducerKind.Init)]
//     public static void Init(ReducerContext ctx) => AuthInstall(ctx);
//
//     static readonly AuthHttpOptions AuthHttp = new() { AppName = "Notes" };
//
//     [HttpHandler]
//     public static HttpResponse AuthPasswordSignupRoute(HandlerContext ctx, HttpRequest req) => AuthPasswordSignup(ctx, req, AuthHttp);
//     // ... one handler per route, then an [HttpRouter] with .Post("/auth/password/signup", Handlers.AuthPasswordSignupRoute) etc.
//
//     [Reducer]
//     public static void CreateNote(ReducerContext ctx, string title)
//     {
//         var userId = AuthRequireCallerUserId(ctx);
//         // ...
//     }
//
// Changes from the TypeScript original:
// - The generic operation names carry the package: link_connection -> auth_link_connection,
//   unlink_connection, update_profile, revoke_session, list_my_sessions, revoke_my_session and
//   whoami likewise. set_auth_config, add_auth_admin, remove_auth_admin, get_auth_public_key,
//   auth_sweep and my_auth_user are unchanged.
// - Procedures return Result<T, string>; reducers throw Exceptions carrying the auth.* codes.
// - ES256 (P-256 ECDSA, RFC 6979 nonces, low-S like noble) and scrypt are implemented here in
//   managed code: C# on wasi has no System.Security.Cryptography. They match the TS output byte
//   for byte (see tests/vectors/auth.json) but are NOT constant-time. Signing runs only with
//   the operator's key on the server; verification handles public data.
// - HTTP handlers are plain methods taking an AuthHttpOptions; the host registers them. OAuth
//   providers are OAuthProviderSpec values; the GitHub resolver reads the user and emails
//   endpoints relative to the spec's UserInfoUrl.
// - Request bodies with non-string email/password/name fields are invalid_request (TS throws a
//   TypeError, a 500). Unparseable token or userinfo responses are 502s with the nearest TS code.
// - AuthVerifyPassword returns false for hashes whose scrypt parameters are invalid or need more
//   than 1 GiB (noble's limit); TS throws.
// - auth_sweep rejects callers other than the scheduler.

#pragma warning disable STDB_UNSTABLE
#nullable enable

using System.Numerics;
using System.Text;
using System.Text.Json;
using SpacetimeDB;

public static class AuthErrors
{
    public const string NotAuthenticated = "auth.not_authenticated";
    public const string NotAuthorized = "auth.not_authorized";
    public const string ConfigMissing = "auth.config_missing";
    public const string SigningKeyRequired = "auth.signing_key_required";
    public const string InvalidPrivateKeyPem = "auth.invalid_private_key_pem";
    public const string InvalidToken = "auth.invalid_token";
    public const string SessionNotFound = "auth.session_not_found";
    public const string SessionNotOwned = "auth.session_not_owned";
    public const string UserNotFound = "auth.user_not_found";
    public const string NameTooLong = "auth.name_too_long";
    public const string ImageTooLong = "auth.image_too_long";
    public const string LastAdmin = "auth.last_admin";
}

[SpacetimeDB.Type]
public partial struct AuthPubKey
{
    public string PublicKeyPem;
    public string KeyId;
    public string IssuerUrl;
}

[SpacetimeDB.Type]
public partial struct MySession
{
    public string SessionId;
    public Timestamp ExpiresAt;
    public Timestamp CreatedAt;
    public string? IpAddress;
    public string? UserAgent;
    public bool IsCurrent;
}

[SpacetimeDB.Type]
public partial struct MySessions
{
    public List<MySession> Sessions;
}

[SpacetimeDB.Type]
public partial struct WhoAmI
{
    public string? UserId;
    public string SenderIdentityHex;
}

/// <summary>A transaction to run auth helpers in: from a reducer, a procedure's or a handler's WithTx.</summary>
public readonly record struct AuthTx(Local Db, Timestamp Timestamp, Identity Sender)
{
    public static AuthTx Of(ReducerContext ctx) => new(ctx.Db, ctx.Timestamp, ctx.Sender);
    public static AuthTx Of(ProcedureTxContext tx) => new(tx.Db, tx.Timestamp, tx.Sender);
    public static AuthTx Of(HandlerTxContext tx) => new(tx.Db, tx.Timestamp, default);
}

/// <summary>Verification and reset mail.</summary>
public sealed record AuthMailParams(string To, string Subject, string Text, string? Html);

public enum AuthTrustedProxyHeader
{
    CfConnectingIp,
    XRealIp,
    XForwardedFor,
}

/// <summary>The TS client(options) settings.</summary>
public sealed class AuthHttpOptions
{
    /// <summary>Enables IP limits and stored session IPs from a trusted proxy header.</summary>
    public AuthTrustedProxyHeader? TrustedProxyHeader { get; init; }
    /// <summary>Default true. Set false only for local plain-HTTP development.</summary>
    public bool SecureCookies { get; init; } = true;
    /// <summary>Verification and reset routes return mailer_not_configured without it.</summary>
    public Action<HandlerContext, AuthMailParams>? SendMail { get; init; }
    public string? AppName { get; init; }
    /// <summary>Where the email verification link lands. Default "/".</summary>
    public string? EmailVerifiedRedirect { get; init; }
}

public sealed record OAuthProfile(string Sub, string Email, bool EmailVerified, string? Name, string? Image);

/// <summary>A sign-in provider. ResolveProfile (if set) replaces the single userinfo request.</summary>
public sealed record OAuthProviderSpec
{
    public required string Id { get; init; }
    public required string AuthorizeUrl { get; init; }
    public required string TokenUrl { get; init; }
    public required string Scope { get; init; }
    public required Func<Module.AuthConfig, string> ClientId { get; init; }
    public required Func<Module.AuthConfig, string> ClientSecret { get; init; }
    /// <summary>Reserved for verified OIDC id_token support. Prefer UserInfoUrl.</summary>
    public bool Oidc { get; init; }
    public string? UserInfoUrl { get; init; }
    public (string Name, string Value)[] UserInfoHeaders { get; init; } = [];
    public required Func<JsonElement, OAuthProfile> ParseProfile { get; init; }
    public Func<HandlerContext, OAuthProviderSpec, string, (OAuthProfile? Profile, string? Error)>? ResolveProfile { get; init; }
    public (string Name, string Value)[] AuthorizeExtras { get; init; } = [];
    /// <summary>Default true.</summary>
    public bool UsePkce { get; init; } = true;
}

/// <summary>An HTTP error: { "error": code } with Status. Thrown inside handlers, turned into a response at the boundary.</summary>
public sealed class AuthHttpException(string code, ushort status) : Exception(code)
{
    public string Code { get; } = code;
    public ushort Status { get; } = status;
}

/// <summary>JS-compatible string helpers.</summary>
public static class AuthJs
{
    /// <summary>JSON.stringify of a string (well-formed: lone surrogates are escaped).</summary>
    public static string Str(string s)
    {
        var sb = new StringBuilder(s.Length + 2);
        sb.Append('"');
        for (var i = 0; i < s.Length; i++)
        {
            var c = s[i];
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
                    if (c < 0x20) sb.Append("\\u").Append(((int)c).ToString("x4"));
                    else if (char.IsHighSurrogate(c) && i + 1 < s.Length && char.IsLowSurrogate(s[i + 1])) sb.Append(c).Append(s[++i]);
                    else if (char.IsSurrogate(c)) sb.Append("\\u").Append(((int)c).ToString("x4"));
                    else sb.Append(c);
                    break;
            }
        }
        sb.Append('"');
        return sb.ToString();
    }

    /// <summary>String.prototype.trim.</summary>
    public static string Trim(string s)
    {
        static bool Ws(char c) => c is (char)0x09 or (char)0x0a or (char)0x0b or (char)0x0c or (char)0x0d or (char)0x20 or (char)0xa0 or (char)0x1680
            or (>= (char)0x2000 and <= (char)0x200a) or (char)0x2028 or (char)0x2029 or (char)0x202f or (char)0x205f or (char)0x3000 or (char)0xfeff;
        int a = 0, b = s.Length;
        while (a < b && Ws(s[a])) a++;
        while (b > a && Ws(s[b - 1])) b--;
        return s[a..b];
    }

    /// <summary>decodeURIComponent; null on malformed escapes or invalid UTF-8.</summary>
    public static string? DecodeUriComponent(string s)
    {
        var bytes = Encoding.UTF8.GetBytes(s);
        var outBytes = new List<byte>(bytes.Length);
        for (var i = 0; i < bytes.Length;)
        {
            if (bytes[i] == (byte)'%')
            {
                if (i + 2 >= bytes.Length || !Uri.IsHexDigit((char)bytes[i + 1]) || !Uri.IsHexDigit((char)bytes[i + 2])) return null;
                outBytes.Add(Convert.ToByte(Encoding.ASCII.GetString(bytes, i + 1, 2), 16));
                i += 3;
            }
            else
            {
                outBytes.Add(bytes[i]);
                i++;
            }
        }
        try
        {
            return new UTF8Encoding(false, true).GetString(outBytes.ToArray());
        }
        catch (ArgumentException)
        {
            return null;
        }
    }

    /// <summary>encodeURIComponent.</summary>
    public static string EncodeUriComponent(string s)
    {
        var sb = new StringBuilder();
        foreach (var b in Encoding.UTF8.GetBytes(s))
        {
            var c = (char)b;
            if (c is (>= 'A' and <= 'Z') or (>= 'a' and <= 'z') or (>= '0' and <= '9') or '-' or '_' or '.' or '!' or '~' or '*' or '\'' or '(' or ')')
                sb.Append(c);
            else sb.Append('%').Append(b.ToString("X2"));
        }
        return sb.ToString();
    }

    /// <summary>parseInt(s, 10): optional whitespace and sign, then the leading digits.</summary>
    public static long? ParseInt(string s)
    {
        var t = s.TrimStart();
        var neg = false;
        if (t.StartsWith('-')) { neg = true; t = t[1..]; }
        else if (t.StartsWith('+')) t = t[1..];
        var n = 0;
        while (n < t.Length && t[n] is >= '0' and <= '9') n++;
        if (n == 0 || n > 18) return n == 0 ? null : long.MaxValue;
        var v = long.Parse(t[..n]);
        return neg ? -v : v;
    }

    /// <summary>String(value ?? '') for a JSON value.</summary>
    public static string StringOf(JsonElement? v)
    {
        if (v is not { } e) return "";
        return e.ValueKind switch
        {
            JsonValueKind.String => e.GetString()!,
            JsonValueKind.Null or JsonValueKind.Undefined => "",
            JsonValueKind.True => "true",
            JsonValueKind.False => "false",
            JsonValueKind.Number => e.TryGetInt64(out var l) ? l.ToString() : e.GetDouble().ToString("R", System.Globalization.CultureInfo.InvariantCulture),
            JsonValueKind.Array => string.Join(",", e.EnumerateArray().Select(x => StringOf(x))),
            _ => "[object Object]",
        };
    }

    public static JsonElement? Prop(JsonElement e, string name) =>
        e.ValueKind == JsonValueKind.Object && e.TryGetProperty(name, out var v) ? v : null;

    public static string? StrProp(JsonElement e, string name) =>
        Prop(e, name) is { ValueKind: JsonValueKind.String } v ? v.GetString() : null;

    public static JsonElement? Parse(string text)
    {
        try
        {
            using var doc = JsonDocument.Parse(text);
            return doc.RootElement.Clone();
        }
        catch (JsonException)
        {
            return null;
        }
    }
}

/// <summary>Base64, scrypt, secrets, ids and PKCE, matching the TS package.</summary>
public static class AuthCrypto
{
    public const int ScryptN = 1 << 14, ScryptR = 8, ScryptP = 1, ScryptDkLen = 32;

    public static string Base64Encode(byte[] bytes) => Convert.ToBase64String(bytes);

    public static string Base64UrlEncode(byte[] bytes) => Convert.ToBase64String(bytes).Replace('+', '-').Replace('/', '_').TrimEnd('=');

    /// <summary>Base64url decode; rejects '+', '/' and '=' like TS. Throws on malformed input.</summary>
    public static byte[] Base64UrlDecode(string value)
    {
        if (value.IndexOfAny(['+', '/', '=']) >= 0) throw new FormatException("base64url: invalid char");
        return Crypto.Base64ToBytes(value.Replace('-', '+').Replace('_', '/'));
    }

    public static string HashPassword(string password, byte[] salt)
    {
        var hash = Scrypt(Encoding.UTF8.GetBytes(password), salt, ScryptN, ScryptR, ScryptP, ScryptDkLen)!;
        return $"scrypt${ScryptN}${ScryptR}${ScryptP}${Base64Encode(salt)}${Base64Encode(hash)}";
    }

    /// <summary>Verified in place of a missing hash, so unknown accounts take as long as known ones.</summary>
    public static string DummyPasswordHash => $"scrypt${ScryptN}${ScryptR}${ScryptP}${Base64Encode(new byte[16])}${Base64Encode(new byte[ScryptDkLen])}";

    public static bool VerifyPassword(string password, string encoded)
    {
        var parts = encoded.Split('$');
        if (parts.Length != 6 || parts[0] != "scrypt") return false;
        var n = AuthJs.ParseInt(parts[1]);
        var r = AuthJs.ParseInt(parts[2]);
        var p = AuthJs.ParseInt(parts[3]);
        if (n is null || r is null || p is null) return false;
        byte[] salt, expected;
        try
        {
            salt = Crypto.Base64ToBytes(parts[4]);
            expected = Crypto.Base64ToBytes(parts[5]);
        }
        catch (Exception)
        {
            return false;
        }
        if (n <= 0 || r <= 0 || p <= 0 || n > int.MaxValue || r > int.MaxValue || p > int.MaxValue) return false;
        var actual = Scrypt(Encoding.UTF8.GetBytes(password), salt, (int)n, (int)r, (int)p, expected.Length);
        return actual is not null && Crypto.TimingSafeEqual(expected, actual);
    }

    public static string PkceChallenge(string verifier) => Base64UrlEncode(Crypto.Sha256(Encoding.UTF8.GetBytes(verifier)));

    public static string UuidV7(ulong nowMs, byte[] rand)
    {
        var b = new byte[16];
        for (var i = 0; i < 6; i++) b[i] = (byte)((nowMs >> (40 - 8 * i)) & 0xff);
        Array.Copy(rand, 0, b, 6, 10);
        b[6] = (byte)((b[6] & 0x0f) | 0x70);
        b[8] = (byte)((b[8] & 0x3f) | 0x80);
        var h = Crypto.BytesToHex(b);
        return $"{h[..8]}-{h[8..12]}-{h[12..16]}-{h[16..20]}-{h[20..]}";
    }

    /// <summary>HMAC-SHA256 keyed by the signing key over a database-wide counter and the timestamp.</summary>
    public static byte[] DeriveSecret(byte[] privateKey, ulong counter, long micros) =>
        Crypto.HmacSha256(privateKey, Encoding.UTF8.GetBytes($"spacetimedb-auth secret {counter} {micros}"));

    // -- scrypt (RFC 7914), managed -------------------------------------------------

    /// <summary>scrypt, or null for invalid parameters (or more than noble's 1 GiB maxmem).</summary>
    public static byte[]? Scrypt(byte[] password, byte[] salt, int n, int r, int p, int dkLen)
    {
        if (n < 2 || (n & (n - 1)) != 0 || r < 1 || p < 1 || dkLen < 1) return null;
        if (128L * r * ((long)n + p) > 1L << 30) return null;
        var blockLen = 128 * r;
        var b = Pbkdf2Sha256Once(password, salt, p * blockLen);
        var x = new uint[32 * r];
        var v = new uint[(long)32 * r * n];
        var y = new uint[32 * r];
        var t = new uint[16];
        for (var i = 0; i < p; i++)
        {
            var off = i * blockLen;
            for (var k = 0; k < 32 * r; k++) x[k] = BitConverter.ToUInt32(b, off + 4 * k);
            for (var j = 0; j < n; j++)
            {
                Array.Copy(x, 0, v, (long)j * 32 * r, 32 * r);
                BlockMix(x, y, t, r);
            }
            for (var j = 0; j < n; j++)
            {
                var idx = (int)(x[(2 * r - 1) * 16] & (uint)(n - 1));
                var vo = (long)idx * 32 * r;
                for (var k = 0; k < 32 * r; k++) x[k] ^= v[vo + k];
                BlockMix(x, y, t, r);
            }
            for (var k = 0; k < 32 * r; k++) BitConverter.TryWriteBytes(b.AsSpan(off + 4 * k), x[k]);
        }
        return Pbkdf2Sha256Once(password, b, dkLen);
    }

    static byte[] Pbkdf2Sha256Once(byte[] password, byte[] salt, int dkLen)
    {
        var output = new byte[dkLen];
        var input = new byte[salt.Length + 4];
        salt.CopyTo(input, 0);
        for (int block = 1, done = 0; done < dkLen; block++)
        {
            input[salt.Length] = (byte)(block >> 24);
            input[salt.Length + 1] = (byte)(block >> 16);
            input[salt.Length + 2] = (byte)(block >> 8);
            input[salt.Length + 3] = (byte)block;
            var u = Crypto.HmacSha256(password, input);
            var take = Math.Min(32, dkLen - done);
            Array.Copy(u, 0, output, done, take);
            done += take;
        }
        return output;
    }

    // X (2r blocks of 16 words) -> BlockMix_salsa20/8, in place; y and t are scratch.
    static void BlockMix(uint[] x, uint[] y, uint[] t, int r)
    {
        Array.Copy(x, (2 * r - 1) * 16, t, 0, 16);
        for (var i = 0; i < 2 * r; i++)
        {
            for (var k = 0; k < 16; k++) t[k] ^= x[i * 16 + k];
            Salsa208(t);
            // Even blocks to the first half, odd blocks to the second.
            var dst = (i / 2 + (i % 2) * r) * 16;
            Array.Copy(t, 0, y, dst, 16);
        }
        Array.Copy(y, x, 32 * r);
    }

    static uint R(uint a, int b) => (a << b) | (a >> (32 - b));

    static void Salsa208(uint[] b)
    {
        uint x0 = b[0], x1 = b[1], x2 = b[2], x3 = b[3], x4 = b[4], x5 = b[5], x6 = b[6], x7 = b[7],
            x8 = b[8], x9 = b[9], x10 = b[10], x11 = b[11], x12 = b[12], x13 = b[13], x14 = b[14], x15 = b[15];
        for (var i = 0; i < 8; i += 2)
        {
            x4 ^= R(x0 + x12, 7); x8 ^= R(x4 + x0, 9); x12 ^= R(x8 + x4, 13); x0 ^= R(x12 + x8, 18);
            x9 ^= R(x5 + x1, 7); x13 ^= R(x9 + x5, 9); x1 ^= R(x13 + x9, 13); x5 ^= R(x1 + x13, 18);
            x14 ^= R(x10 + x6, 7); x2 ^= R(x14 + x10, 9); x6 ^= R(x2 + x14, 13); x10 ^= R(x6 + x2, 18);
            x3 ^= R(x15 + x11, 7); x7 ^= R(x3 + x15, 9); x11 ^= R(x7 + x3, 13); x15 ^= R(x11 + x7, 18);
            x1 ^= R(x0 + x3, 7); x2 ^= R(x1 + x0, 9); x3 ^= R(x2 + x1, 13); x0 ^= R(x3 + x2, 18);
            x6 ^= R(x5 + x4, 7); x7 ^= R(x6 + x5, 9); x4 ^= R(x7 + x6, 13); x5 ^= R(x4 + x7, 18);
            x11 ^= R(x10 + x9, 7); x8 ^= R(x11 + x10, 9); x9 ^= R(x8 + x11, 13); x10 ^= R(x9 + x8, 18);
            x12 ^= R(x15 + x14, 7); x13 ^= R(x12 + x15, 9); x14 ^= R(x13 + x12, 13); x15 ^= R(x14 + x13, 18);
        }
        b[0] += x0; b[1] += x1; b[2] += x2; b[3] += x3; b[4] += x4; b[5] += x5; b[6] += x6; b[7] += x7;
        b[8] += x8; b[9] += x9; b[10] += x10; b[11] += x11; b[12] += x12; b[13] += x13; b[14] += x14; b[15] += x15;
    }
}

/// <summary>
/// P-256 ECDSA (ES256) in managed code: RFC 6979 deterministic nonces with HMAC-SHA256 and
/// low-S signatures, like noble. Not constant-time.
/// </summary>
public static class AuthP256
{
    static readonly BigInteger P = BigInteger.Parse("0ffffffff00000001000000000000000000000000ffffffffffffffffffffffff", System.Globalization.NumberStyles.HexNumber);
    static readonly BigInteger N = BigInteger.Parse("0ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551", System.Globalization.NumberStyles.HexNumber);
    static readonly BigInteger B = BigInteger.Parse("05ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b", System.Globalization.NumberStyles.HexNumber);
    static readonly BigInteger Gx = BigInteger.Parse("06b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296", System.Globalization.NumberStyles.HexNumber);
    static readonly BigInteger Gy = BigInteger.Parse("04fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5", System.Globalization.NumberStyles.HexNumber);
    static readonly BigInteger HalfN = N >> 1;

    readonly record struct Jac(BigInteger X, BigInteger Y, BigInteger Z)
    {
        public bool IsInfinity => Z.IsZero;
    }

    static readonly Jac Infinity = new(BigInteger.One, BigInteger.One, BigInteger.Zero);

    static BigInteger Mod(BigInteger a, BigInteger m)
    {
        var r = a % m;
        return r.Sign < 0 ? r + m : r;
    }

    static BigInteger Inv(BigInteger a, BigInteger m) => BigInteger.ModPow(Mod(a, m), m - 2, m);

    static Jac Double(Jac p)
    {
        if (p.IsInfinity || p.Y.IsZero) return Infinity;
        var delta = Mod(p.Z * p.Z, P);
        var gamma = Mod(p.Y * p.Y, P);
        var beta = Mod(p.X * gamma, P);
        var alpha = Mod(3 * (p.X - delta) * (p.X + delta), P);
        var x3 = Mod(alpha * alpha - 8 * beta, P);
        var z3 = Mod((p.Y + p.Z) * (p.Y + p.Z) - gamma - delta, P);
        var y3 = Mod(alpha * (4 * beta - x3) - 8 * gamma * gamma, P);
        return new Jac(x3, y3, z3);
    }

    static Jac Add(Jac a, Jac b)
    {
        if (a.IsInfinity) return b;
        if (b.IsInfinity) return a;
        var z1z1 = Mod(a.Z * a.Z, P);
        var z2z2 = Mod(b.Z * b.Z, P);
        var u1 = Mod(a.X * z2z2, P);
        var u2 = Mod(b.X * z1z1, P);
        var s1 = Mod(a.Y * b.Z * z2z2, P);
        var s2 = Mod(b.Y * a.Z * z1z1, P);
        if (u1 == u2) return s1 == s2 ? Double(a) : Infinity;
        var h = Mod(u2 - u1, P);
        var i = Mod(4 * h * h, P);
        var j = Mod(h * i, P);
        var r = Mod(2 * (s2 - s1), P);
        var v = Mod(u1 * i, P);
        var x3 = Mod(r * r - j - 2 * v, P);
        var y3 = Mod(r * (v - x3) - 2 * s1 * j, P);
        var z3 = Mod(((a.Z + b.Z) * (a.Z + b.Z) - z1z1 - z2z2) * h, P);
        return new Jac(x3, y3, z3);
    }

    static Jac Mul(BigInteger k, Jac p)
    {
        var acc = Infinity;
        for (var i = (int)k.GetBitLength() - 1; i >= 0; i--)
        {
            acc = Double(acc);
            if (!(k >> i).IsEven) acc = Add(acc, p);
        }
        return acc;
    }

    static (BigInteger X, BigInteger Y) Affine(Jac p)
    {
        var zi = Inv(p.Z, P);
        var zi2 = Mod(zi * zi, P);
        return (Mod(p.X * zi2, P), Mod(p.Y * zi2 * zi, P));
    }

    static Jac G => new(Gx, Gy, BigInteger.One);

    static BigInteger FromBytes(ReadOnlySpan<byte> b) => new(b, isUnsigned: true, isBigEndian: true);

    static byte[] ToBytes32(BigInteger v)
    {
        var raw = v.ToByteArray(isUnsigned: true, isBigEndian: true);
        if (raw.Length > 32) throw new ArgumentException("value too large");
        var out32 = new byte[32];
        raw.CopyTo(out32, 32 - raw.Length);
        return out32;
    }

    static BigInteger PrivateScalar(byte[] priv)
    {
        if (priv.Length != 32) throw new ArgumentException("ES256 private key must be 32 bytes");
        var d = FromBytes(priv);
        if (d.IsZero || d >= N) throw new ArgumentException("invalid private key");
        return d;
    }

    /// <summary>The 65-byte uncompressed public key.</summary>
    public static byte[] GetPublicKey(byte[] priv)
    {
        var (x, y) = Affine(Mul(PrivateScalar(priv), G));
        return [0x04, .. ToBytes32(x), .. ToBytes32(y)];
    }

    /// <summary>64-byte r||s over SHA-256(msg), RFC 6979 nonce, low-S.</summary>
    public static byte[] Sign(byte[] msg, byte[] priv)
    {
        var d = PrivateScalar(priv);
        var h = Mod(FromBytes(Crypto.Sha256(msg)), N);
        var x = ToBytes32(d);
        var hb = ToBytes32(h);
        var v = Enumerable.Repeat((byte)1, 32).ToArray();
        var k = new byte[32];
        k = Crypto.HmacSha256(k, [.. v, 0x00, .. x, .. hb]);
        v = Crypto.HmacSha256(k, v);
        k = Crypto.HmacSha256(k, [.. v, 0x01, .. x, .. hb]);
        v = Crypto.HmacSha256(k, v);
        for (;;)
        {
            v = Crypto.HmacSha256(k, v);
            var kk = FromBytes(v);
            if (!kk.IsZero && kk < N)
            {
                var (rx, _) = Affine(Mul(kk, G));
                var r = Mod(rx, N);
                if (!r.IsZero)
                {
                    var s = Mod(Inv(kk, N) * (h + r * d), N);
                    if (!s.IsZero)
                    {
                        if (s > HalfN) s = N - s;
                        return [.. ToBytes32(r), .. ToBytes32(s)];
                    }
                }
            }
            k = Crypto.HmacSha256(k, [.. v, 0x00]);
            v = Crypto.HmacSha256(k, v);
        }
    }

    /// <summary>Verify a 64-byte r||s signature over SHA-256(msg). Rejects high-S like noble.</summary>
    public static bool Verify(byte[] sig, byte[] msg, byte[] pub)
    {
        if (sig.Length != 64 || pub.Length != 65 || pub[0] != 0x04) return false;
        var qx = FromBytes(pub.AsSpan(1, 32));
        var qy = FromBytes(pub.AsSpan(33, 32));
        if (qx >= P || qy >= P) return false;
        if (Mod(qy * qy - (qx * qx * qx - 3 * qx + B), P) != 0) return false;
        var r = FromBytes(sig.AsSpan(0, 32));
        var s = FromBytes(sig.AsSpan(32, 32));
        if (r.IsZero || r >= N || s.IsZero || s >= N || s > HalfN) return false;
        var e = Mod(FromBytes(Crypto.Sha256(msg)), N);
        var w = Inv(s, N);
        var sum = Add(Mul(Mod(e * w, N), G), Mul(Mod(r * w, N), new Jac(qx, qy, BigInteger.One)));
        if (sum.IsInfinity) return false;
        var (x, _) = Affine(sum);
        return Mod(x, N) == r;
    }
}

/// <summary>ES256 key encodings (PKCS#8, SPKI, RFC 7638 kid), byte-identical to the TS package.</summary>
public sealed record AuthKeypair(byte[] PrivateKey, byte[] PublicKey, string PrivateKeyPem, string PublicKeyPem, string Kid)
{
    static readonly byte[] SpkiAlgDer = [0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];

    public static AuthKeypair FromPrivateKeyBytes(byte[] privateKey)
    {
        if (privateKey.Length != 32) throw new ArgumentException("ES256 private key must be 32 bytes");
        var pub = AuthP256.GetPublicKey(privateKey);
        return new(privateKey, pub, EncodePrivateKeyPem(privateKey, pub), EncodePublicKeyPem(pub), Thumbprint(pub));
    }

    static byte[] LenPrefix(byte tag, int len) =>
        len < 0x80 ? [tag, (byte)len] : len < 0x100 ? [tag, 0x81, (byte)len] : [tag, 0x82, (byte)(len >> 8), (byte)len];

    static byte[] Seq(byte[] body) => [.. LenPrefix(0x30, body.Length), .. body];

    static byte[] BitString(byte[] pub) => [0x03, (byte)(pub.Length + 1), 0x00, .. pub];

    static string PemWrap(string label, byte[] der)
    {
        var b64 = Convert.ToBase64String(der);
        var lines = Enumerable.Range(0, (b64.Length + 63) / 64).Select(i => b64.Substring(i * 64, Math.Min(64, b64.Length - i * 64)));
        return $"-----BEGIN {label}-----\n{string.Join("\n", lines)}\n-----END {label}-----\n";
    }

    static string EncodePublicKeyPem(byte[] pub) => PemWrap("PUBLIC KEY", Seq([.. SpkiAlgDer, .. BitString(pub)]));

    static string EncodePrivateKeyPem(byte[] priv, byte[] pub)
    {
        byte[] namedCurve = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
        var pubBits = BitString(pub);
        var ecDer = Seq([0x02, 0x01, 0x01, 0x04, (byte)priv.Length, .. priv, 0xa0, (byte)namedCurve.Length, .. namedCurve, 0xa1, (byte)pubBits.Length, .. pubBits]);
        return PemWrap("PRIVATE KEY", Seq([0x02, 0x01, 0x00, .. SpkiAlgDer, .. LenPrefix(0x04, ecDer.Length), .. ecDer]));
    }

    static string Thumbprint(byte[] pub)
    {
        var x = AuthCrypto.Base64UrlEncode(pub[1..33]);
        var y = AuthCrypto.Base64UrlEncode(pub[33..65]);
        return AuthCrypto.Base64UrlEncode(Crypto.Sha256(Encoding.UTF8.GetBytes($"{{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"{x}\",\"y\":\"{y}\"}}")));
    }

    static byte[] PemUnwrap(string label, string pem)
    {
        var prefix = $"-----BEGIN {label}-----";
        var suffix = $"-----END {label}-----";
        var start = pem.IndexOf(prefix, StringComparison.Ordinal);
        var end = pem.IndexOf(suffix, StringComparison.Ordinal);
        if (start < 0 || end < 0 || end < start + prefix.Length) throw new FormatException($"PEM {label} block not found");
        var inner = new string(pem[(start + prefix.Length)..end].Where(c => !char.IsWhiteSpace(c)).ToArray());
        return Crypto.Base64ToBytes(inner);
    }

    /// <summary>The raw 32-byte key from a PKCS#8 PEM (the first 04 20 octet string, as TS).</summary>
    public static byte[] PrivateKeyFromPem(string pem)
    {
        var der = PemUnwrap("PRIVATE KEY", pem);
        for (var i = 0; i < der.Length - 33; i++)
        {
            if (der[i] == 0x04 && der[i + 1] == 0x20) return der[(i + 2)..(i + 34)];
        }
        throw new FormatException("could not extract raw private key from PKCS#8 PEM");
    }

    /// <summary>The 65-byte point from an SPKI PEM.</summary>
    public static byte[] PublicKeyFromPem(string pem)
    {
        var der = PemUnwrap("PUBLIC KEY", pem);
        for (var i = 0; i < der.Length - 67; i++)
        {
            if (der[i] == 0x03 && der[i + 1] == 0x42 && der[i + 2] == 0x00 && der[i + 3] == 0x04) return der[(i + 3)..(i + 68)];
        }
        throw new FormatException("could not extract raw public key from SPKI PEM");
    }
}

/// <summary>The outcome of AuthJwt.Verify: Ok with the claims and header, or a reason.</summary>
public sealed record AuthJwtResult(bool Ok, string? Reason, JsonElement Claims, JsonElement Header);

public static class AuthJwt
{
    /// <summary>Sign header.payload; payloadJson is the exact claims text.</summary>
    public static string Sign(byte[] privateKey, string payloadJson, string? kid)
    {
        var header = kid is null ? "{\"alg\":\"ES256\",\"typ\":\"JWT\"}" : $"{{\"alg\":\"ES256\",\"typ\":\"JWT\",\"kid\":{AuthJs.Str(kid)}}}";
        var input = $"{AuthCrypto.Base64UrlEncode(Encoding.UTF8.GetBytes(header))}.{AuthCrypto.Base64UrlEncode(Encoding.UTF8.GetBytes(payloadJson))}";
        return $"{input}.{AuthCrypto.Base64UrlEncode(AuthP256.Sign(Encoding.UTF8.GetBytes(input), privateKey))}";
    }

    /// <summary>The claims of an auth session token, in the TS key order.</summary>
    public static string SessionClaims(string iss, string sub, long iat, long exp, string jti) =>
        $"{{\"iss\":{AuthJs.Str(iss)},\"sub\":{AuthJs.Str(sub)},\"aud\":{AuthJs.Str(iss)},\"iat\":{iat},\"exp\":{exp},\"jti\":{AuthJs.Str(jti)}}}";

    static AuthJwtResult Fail(string reason) => new(false, reason, default, default);

    /// <summary>Like TS verifyJwt: malformed, bad-signature, expired, not-yet-valid, bad-issuer, bad-audience.</summary>
    public static AuthJwtResult Verify(byte[] publicKey, string token, long nowSeconds, string? issuer = null, string? audience = null, long clockToleranceSeconds = 60)
    {
        var parts = token.Split('.');
        if (parts.Length != 3) return Fail("malformed");
        JsonElement? Decode(string p)
        {
            try
            {
                return AuthJs.Parse(new UTF8Encoding(false, false).GetString(AuthCrypto.Base64UrlDecode(p)));
            }
            catch (Exception)
            {
                return null;
            }
        }
        if (Decode(parts[0]) is not { } header || Decode(parts[1]) is not { } claims) return Fail("malformed");
        if (AuthJs.StrProp(header, "alg") != "ES256") return Fail("bad-signature");
        byte[] sig;
        try
        {
            sig = AuthCrypto.Base64UrlDecode(parts[2]);
        }
        catch (Exception)
        {
            return Fail("malformed");
        }
        if (sig.Length != 64) return Fail("bad-signature");
        bool ok;
        try
        {
            ok = AuthP256.Verify(sig, Encoding.UTF8.GetBytes($"{parts[0]}.{parts[1]}"), publicKey);
        }
        catch (Exception)
        {
            ok = false;
        }
        if (!ok) return Fail("bad-signature");
        double? Num(string name) => AuthJs.Prop(claims, name) is { ValueKind: JsonValueKind.Number } v ? v.GetDouble() : null;
        if (Num("exp") is { } exp && exp + clockToleranceSeconds < nowSeconds) return Fail("expired");
        if (Num("nbf") is { } nbf && nbf - clockToleranceSeconds > nowSeconds) return Fail("not-yet-valid");
        if (issuer is not null && AuthJs.StrProp(claims, "iss") != issuer) return Fail("bad-issuer");
        if (audience is not null)
        {
            var aud = AuthJs.Prop(claims, "aud");
            var matches = aud switch
            {
                { ValueKind: JsonValueKind.Array } a => a.EnumerateArray().Any(x => x.ValueKind == JsonValueKind.String && x.GetString() == audience),
                { ValueKind: JsonValueKind.String } s => s.GetString() == audience,
                _ => false,
            };
            if (!matches) return Fail("bad-audience");
        }
        return new(true, null, claims, header);
    }
}

public static partial class Module
{
    const long AuthSecondMicros = 1_000_000;
    const string AuthDefaultCookieName = "stdb_auth";
    const ulong AuthDefaultSessionTtlSeconds = 60 * 60 * 24 * 7;
    const int AuthMaxProfileNameLen = 64, AuthMaxProfileImageLen = 2048;
    public const int AuthMinPasswordLen = 8, AuthMaxPasswordLen = 1024, AuthMaxEmailLen = 320;
    const int AuthMaxSignupNameLen = 128, AuthMaxTokenLen = 256;
    const long AuthResetTokenTtl = 60 * 60, AuthVerifyTokenTtl = 60 * 60 * 24, AuthOauthStateTtl = 600;
    const int AuthMaxOauthCodeLength = 4096, AuthMaxOauthStateLength = 256, AuthMaxProfileSubLength = 512,
        AuthMaxProfileEmailLength = 320, AuthMaxOauthProfileNameLength = 256, AuthMaxOauthProfileImageLength = 2048;
    static readonly Timestamp AuthTimestampMin = new(long.MinValue);

    // -----------------------------------------------------------------------
    // Tables
    // -----------------------------------------------------------------------

    [SpacetimeDB.Table(Accessor = "AuthUser")]
    public partial struct AuthUser
    {
        [SpacetimeDB.PrimaryKey]
        public string UserId;
        [SpacetimeDB.Unique]
        public string Email;
        public bool EmailVerified;
        public string? Name;
        public string? Image;
        public Timestamp CreatedAt;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "AuthSession")]
    public partial struct AuthSession
    {
        [SpacetimeDB.PrimaryKey]
        public string SessionId;
        [SpacetimeDB.Index.BTree]
        public string UserId;
        [SpacetimeDB.Index.BTree]
        public Timestamp ExpiresAt;
        public string? IpAddress;
        public string? UserAgent;
        public Timestamp CreatedAt;
    }

    /// <summary>ProviderId: password, google, github, ...; ProviderAccountId: email or provider sub.</summary>
    [SpacetimeDB.Table(Accessor = "AuthAccount")]
    public partial struct AuthAccount
    {
        [SpacetimeDB.PrimaryKey]
        public string AccountId;
        [SpacetimeDB.Index.BTree]
        public string UserId;
        [SpacetimeDB.Index.BTree]
        public string ProviderId;
        [SpacetimeDB.Index.BTree]
        public string ProviderAccountId;
        public string? PasswordHash;
        public string? AccessToken;
        public string? RefreshToken;
        public Timestamp? AccessTokenExpiresAt;
        public Timestamp CreatedAt;
        public Timestamp UpdatedAt;
    }

    [SpacetimeDB.Table(Accessor = "AuthVerification")]
    public partial struct AuthVerification
    {
        [SpacetimeDB.PrimaryKey]
        public string VerificationId;
        [SpacetimeDB.Index.BTree]
        public string Identifier;
        [SpacetimeDB.Unique]
        public string Value;
        public string Purpose;
        [SpacetimeDB.Index.BTree]
        public Timestamp ExpiresAt;
        public Timestamp CreatedAt;
    }

    [SpacetimeDB.Table(Accessor = "AuthOauthState")]
    public partial struct AuthOauthState
    {
        [SpacetimeDB.PrimaryKey]
        public string State;
        public string Provider;
        public string CodeVerifier;
        public string RedirectTo;
        [SpacetimeDB.Index.BTree]
        public Timestamp ExpiresAt;
        public Timestamp CreatedAt;
    }

    /// <summary>Private singleton, populated by set_auth_config.</summary>
    [SpacetimeDB.Table(Accessor = "AuthConfig")]
    public partial struct AuthConfig
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public string IssuerUrl;
        public string BaseUrl;
        public string CookieName;
        public ulong SessionTtlSeconds;
        public string Es256PrivateKeyPem;
        public string Es256PublicKeyPem;
        public string KeyId;
        /// <summary>Advances on every secret draw (see AuthSecretBytes).</summary>
        public ulong TokenCounter;
        public string? GoogleClientId;
        public string? GoogleClientSecret;
        public string? GithubClientId;
        public string? GithubClientSecret;
        public Timestamp UpdatedAt;
    }

    /// <summary>Maps a SpacetimeDB identity to the session it presented to auth_link_connection.</summary>
    [SpacetimeDB.Table(Accessor = "AuthConnectionBinding")]
    public partial struct AuthConnectionBinding
    {
        [SpacetimeDB.PrimaryKey]
        public Identity StdbIdentity;
        [SpacetimeDB.Index.BTree]
        public string UserId;
        [SpacetimeDB.Index.BTree]
        public string SessionId;
        public Timestamp LinkedAt;
    }

    /// <summary>Operator allowlist; AuthInstall seeds the publishing identity.</summary>
    [SpacetimeDB.Table(Accessor = "AuthAdminIdentity")]
    public partial struct AuthAdminIdentity
    {
        [SpacetimeDB.PrimaryKey]
        public Identity Identity;
        public Timestamp AddedAt;
    }

    [SpacetimeDB.Table(Accessor = "AuthSweeperTick", Scheduled = nameof(AuthSweep), ScheduledAt = nameof(AuthSweeperTick.ScheduledAt))]
    public partial struct AuthSweeperTick
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
    }

    static long AuthMicros(Timestamp t) => t.MicrosecondsSinceUnixEpoch;
    static Timestamp AuthPlusSeconds(Timestamp t, long secs) => new(t.MicrosecondsSinceUnixEpoch + secs * AuthSecondMicros);
    static long AuthSeconds(Timestamp t) => (long)Math.Floor(t.MicrosecondsSinceUnixEpoch / (double)AuthSecondMicros);

    // -----------------------------------------------------------------------
    // Install, admin, config
    // -----------------------------------------------------------------------

    /// <summary>Call from your Init reducer: installs rate-limit, makes the publisher the first admin, schedules the 60 s sweep.</summary>
    public static void AuthInstall(ReducerContext ctx)
    {
        RateLimitInstall(ctx);
        if (ctx.Db.AuthAdminIdentity.Identity.Find(ctx.Sender) is null)
        {
            ctx.Db.AuthAdminIdentity.Insert(new AuthAdminIdentity { Identity = ctx.Sender, AddedAt = ctx.Timestamp });
        }
        if (ctx.Db.AuthSweeperTick.Count == 0)
        {
            ctx.Db.AuthSweeperTick.Insert(new AuthSweeperTick { ScheduledId = 0, ScheduledAt = new ScheduleAt.Interval(new TimeDuration(60 * AuthSecondMicros)) });
        }
    }

    static void AuthRequireAdmin(ReducerContext ctx)
    {
        if (ctx.Db.AuthAdminIdentity.Identity.Find(ctx.Sender) is null) throw new Exception(AuthErrors.NotAuthorized);
    }

    /// <summary>Admin only. The first call needs es256PrivateKeyPem (PKCS#8, generated outside the module).</summary>
    [SpacetimeDB.Reducer]
    public static void SetAuthConfig(ReducerContext ctx, string issuerUrl, string? baseUrl, string? cookieName, ulong? sessionTtlSeconds, string? es256PrivateKeyPem,
        string? googleClientId, string? googleClientSecret, string? githubClientId, string? githubClientSecret)
    {
        AuthRequireAdmin(ctx);
        var existing = ctx.Db.AuthConfig.Singleton.Find(true);
        (string Priv, string Pub, string Kid)? key = existing is { } e ? (e.Es256PrivateKeyPem, e.Es256PublicKeyPem, e.KeyId) : null;
        if (!string.IsNullOrEmpty(es256PrivateKeyPem))
        {
            try
            {
                var kp = AuthKeypair.FromPrivateKeyBytes(AuthKeypair.PrivateKeyFromPem(es256PrivateKeyPem));
                key = (kp.PrivateKeyPem, kp.PublicKeyPem, kp.Kid);
            }
            catch (Exception ex)
            {
                throw new Exception($"{AuthErrors.InvalidPrivateKeyPem}:{ex.Message}");
            }
        }
        if (key is not { } k) throw new Exception(AuthErrors.SigningKeyRequired);
        var row = new AuthConfig
        {
            Singleton = true,
            IssuerUrl = issuerUrl,
            BaseUrl = baseUrl ?? existing?.BaseUrl ?? issuerUrl,
            CookieName = cookieName ?? existing?.CookieName ?? AuthDefaultCookieName,
            SessionTtlSeconds = sessionTtlSeconds ?? existing?.SessionTtlSeconds ?? AuthDefaultSessionTtlSeconds,
            Es256PrivateKeyPem = k.Priv,
            Es256PublicKeyPem = k.Pub,
            KeyId = k.Kid,
            TokenCounter = existing?.TokenCounter ?? 0,
            GoogleClientId = googleClientId ?? existing?.GoogleClientId,
            GoogleClientSecret = googleClientSecret ?? existing?.GoogleClientSecret,
            GithubClientId = githubClientId ?? existing?.GithubClientId,
            GithubClientSecret = githubClientSecret ?? existing?.GithubClientSecret,
            UpdatedAt = ctx.Timestamp,
        };
        if (existing is not null) ctx.Db.AuthConfig.Singleton.Update(row);
        else ctx.Db.AuthConfig.Insert(row);
    }

    [SpacetimeDB.Reducer]
    public static void AddAuthAdmin(ReducerContext ctx, Identity identity)
    {
        AuthRequireAdmin(ctx);
        if (ctx.Db.AuthAdminIdentity.Identity.Find(identity) is null)
        {
            ctx.Db.AuthAdminIdentity.Insert(new AuthAdminIdentity { Identity = identity, AddedAt = ctx.Timestamp });
        }
    }

    [SpacetimeDB.Reducer]
    public static void RemoveAuthAdmin(ReducerContext ctx, Identity identity)
    {
        AuthRequireAdmin(ctx);
        if (ctx.Db.AuthAdminIdentity.Identity.Find(identity) is null) return;
        if (ctx.Db.AuthAdminIdentity.Count <= 1) throw new Exception(AuthErrors.LastAdmin);
        ctx.Db.AuthAdminIdentity.Identity.Delete(identity);
    }

    static Result<T, string> AuthTry<T>(Func<T> f)
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
    public static Result<AuthPubKey, string> GetAuthPublicKey(ProcedureContext ctx) => AuthTry(() => ctx.WithTx(tx =>
    {
        var cfg = tx.Db.AuthConfig.Singleton.Find(true) ?? throw new Exception(AuthErrors.ConfigMissing);
        return new AuthPubKey { PublicKeyPem = cfg.Es256PublicKeyPem, KeyId = cfg.KeyId, IssuerUrl = cfg.IssuerUrl };
    }));

    // -----------------------------------------------------------------------
    // Secrets, sessions and the caller
    // -----------------------------------------------------------------------

    /// <summary>32 bytes that are unpredictable without the signing key. Advances the config counter.</summary>
    public static byte[] AuthSecretBytes(AuthTx tx)
    {
        var cfg = tx.Db.AuthConfig.Singleton.Find(true) ?? throw new Exception(AuthErrors.ConfigMissing);
        var counter = cfg.TokenCounter + 1;
        var key = AuthKeypair.PrivateKeyFromPem(cfg.Es256PrivateKeyPem);
        tx.Db.AuthConfig.Singleton.Update(cfg with { TokenCounter = counter });
        return AuthCrypto.DeriveSecret(key, counter, AuthMicros(tx.Timestamp));
    }

    public static string AuthNewToken(AuthTx tx) => AuthCrypto.Base64UrlEncode(AuthSecretBytes(tx));

    public static string AuthNewId(AuthTx tx) => AuthCrypto.UuidV7((ulong)Math.Max(0, AuthMicros(tx.Timestamp) / 1000), AuthSecretBytes(tx));

    /// <summary>Deletes a session and every connection bound to it.</summary>
    public static void AuthDeleteSession(AuthTx tx, AuthSession session)
    {
        foreach (var b in tx.Db.AuthConnectionBinding.SessionId.Filter(session.SessionId).ToList())
        {
            tx.Db.AuthConnectionBinding.StdbIdentity.Delete(b.StdbIdentity);
        }
        tx.Db.AuthSession.SessionId.Delete(session.SessionId);
    }

    public static bool AuthIsLive(AuthSession s, Timestamp now) => AuthMicros(s.ExpiresAt) > AuthMicros(now);

    /// <summary>The live session for a signed session token.</summary>
    public static AuthSession? AuthTokenSession(AuthTx tx, AuthConfig cfg, string token)
    {
        byte[] pub;
        try
        {
            pub = AuthKeypair.PublicKeyFromPem(cfg.Es256PublicKeyPem);
        }
        catch (Exception)
        {
            return null;
        }
        var v = AuthJwt.Verify(pub, token, AuthSeconds(tx.Timestamp), cfg.IssuerUrl);
        if (!v.Ok) return null;
        var jti = AuthJs.StrProp(v.Claims, "jti");
        if (string.IsNullOrEmpty(jti)) return null;
        if (tx.Db.AuthSession.SessionId.Find(jti) is not { } session) return null;
        if (AuthJs.StrProp(v.Claims, "sub") != session.UserId) return null;
        return AuthIsLive(session, tx.Timestamp) ? session : null;
    }

    /// <summary>The live session presented by a request's bearer token or session cookie.</summary>
    public static AuthSession? AuthRequestSession(AuthTx tx, AuthConfig cfg, HttpRequest req) =>
        AuthReadBearer(req, cfg.CookieName) is { } token ? AuthTokenSession(tx, cfg, token) : null;

    /// <summary>The user id of the live session presented to an HTTP handler (whose sender is a new identity on every request).</summary>
    public static string? AuthRequestUserId(AuthTx tx, HttpRequest req) =>
        tx.Db.AuthConfig.Singleton.Find(true) is { } cfg ? AuthRequestSession(tx, cfg, req)?.UserId : null;

    /// <summary>The caller's auth_user row if its connection is bound to a live session.</summary>
    public static AuthUser? AuthFindCallerUser(AuthTx tx)
    {
        if (tx.Db.AuthConnectionBinding.StdbIdentity.Find(tx.Sender) is not { } binding) return null;
        if (tx.Db.AuthSession.SessionId.Find(binding.SessionId) is not { } s || !AuthIsLive(s, tx.Timestamp)) return null;
        return tx.Db.AuthUser.UserId.Find(binding.UserId);
    }

    /// <summary>From a view: views have no clock, so an expired session stops counting within one sweep.</summary>
    public static AuthUser? AuthFindCallerUser(ViewContext ctx)
    {
        if (ctx.Db.AuthConnectionBinding.StdbIdentity.Find(ctx.Sender) is not { } binding) return null;
        if (ctx.Db.AuthSession.SessionId.Find(binding.SessionId) is null) return null;
        return ctx.Db.AuthUser.UserId.Find(binding.UserId);
    }

    public static string? AuthGetCallerUserId(ReducerContext ctx) => AuthFindCallerUser(AuthTx.Of(ctx))?.UserId;

    /// <summary>The caller's user id, or throws auth.not_authenticated.</summary>
    public static string AuthRequireCallerUserId(ReducerContext ctx) => AuthGetCallerUserId(ctx) ?? throw new Exception(AuthErrors.NotAuthenticated);

    // -----------------------------------------------------------------------
    // Client operations
    // -----------------------------------------------------------------------

    /// <summary>Call once after each connect: binds this connection to the session behind sessionToken.</summary>
    [SpacetimeDB.Reducer]
    public static void AuthLinkConnection(ReducerContext ctx, string sessionToken)
    {
        var cfg = ctx.Db.AuthConfig.Singleton.Find(true) ?? throw new Exception(AuthErrors.ConfigMissing);
        var session = AuthTokenSession(AuthTx.Of(ctx), cfg, sessionToken) ?? throw new Exception(AuthErrors.InvalidToken);
        var binding = new AuthConnectionBinding { StdbIdentity = ctx.Sender, UserId = session.UserId, SessionId = session.SessionId, LinkedAt = ctx.Timestamp };
        if (ctx.Db.AuthConnectionBinding.StdbIdentity.Find(ctx.Sender) is not null) ctx.Db.AuthConnectionBinding.StdbIdentity.Update(binding);
        else ctx.Db.AuthConnectionBinding.Insert(binding);
    }

    [SpacetimeDB.Reducer]
    public static void AuthUnlinkConnection(ReducerContext ctx) => ctx.Db.AuthConnectionBinding.StdbIdentity.Delete(ctx.Sender);

    /// <summary>Set the caller's display name / image; an empty string clears it, null keeps it.</summary>
    [SpacetimeDB.Reducer]
    public static void AuthUpdateProfile(ReducerContext ctx, string? name, string? image)
    {
        var userId = AuthRequireCallerUserId(ctx);
        var user = ctx.Db.AuthUser.UserId.Find(userId) ?? throw new Exception(AuthErrors.UserNotFound);
        static string? Next(string? value, string? current) => value is null ? current : AuthJs.Trim(value) is { Length: > 0 } t ? t : null;
        var n = Next(name, user.Name);
        var i = Next(image, user.Image);
        if (n is not null && n.Length > AuthMaxProfileNameLen) throw new Exception($"{AuthErrors.NameTooLong}:max={AuthMaxProfileNameLen}");
        if (i is not null && i.Length > AuthMaxProfileImageLen) throw new Exception($"{AuthErrors.ImageTooLong}:max={AuthMaxProfileImageLen}");
        ctx.Db.AuthUser.UserId.Update(user with { Name = n, Image = i, UpdatedAt = ctx.Timestamp });
    }

    /// <summary>Admin action for revoking any user's session.</summary>
    [SpacetimeDB.Reducer]
    public static void AuthRevokeSession(ReducerContext ctx, string sessionId)
    {
        AuthRequireAdmin(ctx);
        if (ctx.Db.AuthSession.SessionId.Find(sessionId) is { } s) AuthDeleteSession(AuthTx.Of(ctx), s);
    }

    [SpacetimeDB.Reducer]
    public static void AuthRevokeMySession(ReducerContext ctx, string sessionId)
    {
        var userId = AuthRequireCallerUserId(ctx);
        if (ctx.Db.AuthSession.SessionId.Find(sessionId) is not { } s) return;
        if (s.UserId != userId) throw new Exception(AuthErrors.SessionNotOwned);
        AuthDeleteSession(AuthTx.Of(ctx), s);
    }

    [SpacetimeDB.Procedure]
    public static Result<MySessions, string> AuthListMySessions(ProcedureContext ctx) => AuthTry(() => ctx.WithTx(tx =>
    {
        var empty = new MySessions { Sessions = [] };
        if (tx.Db.AuthConnectionBinding.StdbIdentity.Find(ctx.Sender) is not { } binding) return empty;
        if (tx.Db.AuthSession.SessionId.Find(binding.SessionId) is not { } current || !AuthIsLive(current, tx.Timestamp)) return empty;
        var sessions = tx.Db.AuthSession.UserId.Filter(binding.UserId)
            .Where(s => AuthIsLive(s, tx.Timestamp))
            .Select(s => new MySession { SessionId = s.SessionId, ExpiresAt = s.ExpiresAt, CreatedAt = s.CreatedAt, IpAddress = s.IpAddress, UserAgent = s.UserAgent, IsCurrent = s.SessionId == binding.SessionId })
            // Newest first; OrderBy is stable, like Array.prototype.sort.
            .OrderByDescending(s => AuthMicros(s.CreatedAt))
            .ToList();
        return new MySessions { Sessions = sessions };
    }));

    [SpacetimeDB.Procedure]
    public static Result<WhoAmI, string> AuthWhoami(ProcedureContext ctx) => AuthTry(() =>
    {
        var userId = ctx.WithTx(tx => AuthFindCallerUser(AuthTx.Of(tx))?.UserId);
        return new WhoAmI { UserId = userId, SenderIdentityHex = ctx.Sender.ToString().ToLowerInvariant().Replace("0x", "") };
    });

    [SpacetimeDB.View(Accessor = "MyAuthUser", Public = true)]
    public static List<AuthUser> MyAuthUser(ViewContext ctx) => AuthFindCallerUser(ctx) is { } u ? [u] : [];

    /// <summary>Deletes every expired session, verification and OAuth state. Scheduled every 60 s.</summary>
    [SpacetimeDB.Reducer]
    public static void AuthSweep(ReducerContext ctx, AuthSweeperTick tick)
    {
        if (ctx.Sender != ctx.DatabaseIdentity) throw new Exception(AuthErrors.NotAuthorized);
        var tx = AuthTx.Of(ctx);
        var expired = new Bound<Timestamp>(AuthTimestampMin, ctx.Timestamp);
        foreach (var s in ctx.Db.AuthSession.ExpiresAt.Filter(expired).ToList()) AuthDeleteSession(tx, s);
        foreach (var v in ctx.Db.AuthVerification.ExpiresAt.Filter(expired).ToList()) ctx.Db.AuthVerification.VerificationId.Delete(v.VerificationId);
        foreach (var s in ctx.Db.AuthOauthState.ExpiresAt.Filter(expired).ToList()) ctx.Db.AuthOauthState.State.Delete(s.State);
    }

    // -----------------------------------------------------------------------
    // HTTP plumbing
    // -----------------------------------------------------------------------

    public static string AuthMakeCookie(string name, string value, long? maxAgeSeconds, bool secure)
    {
        var parts = new List<string> { $"{name}={value}", "Path=/" };
        if (maxAgeSeconds is { } m) parts.Add($"Max-Age={m}");
        parts.Add("HttpOnly");
        if (secure) parts.Add("Secure");
        parts.Add("SameSite=Lax");
        return string.Join("; ", parts);
    }

    public static string AuthClearCookie(string name, bool secure) => AuthMakeCookie(name, "", 0, secure);

    /// <summary>headers.get(name): every value for the name, joined with ", ".</summary>
    public static string? AuthHeader(HttpRequest req, string name)
    {
        var values = req.Headers.Where(h => string.Equals(h.Name, name, StringComparison.OrdinalIgnoreCase)).Select(h => Encoding.UTF8.GetString(h.Value)).ToList();
        return values.Count == 0 ? null : string.Join(", ", values);
    }

    public static Dictionary<string, string> AuthParseCookies(string? header)
    {
        var o = new Dictionary<string, string>();
        if (header is null) return o;
        foreach (var part in header.Split(';'))
        {
            var eq = part.IndexOf('=');
            if (eq < 0) continue;
            var k = AuthJs.Trim(part[..eq]);
            if (k.Length > 0) o[k] = AuthJs.Trim(part[(eq + 1)..]);
        }
        return o;
    }

    static string? AuthCookie(HttpRequest req, string name) => AuthParseCookies(AuthHeader(req, "cookie")).GetValueOrDefault(name);

    /// <summary>Bearer token, else the session cookie.</summary>
    public static string? AuthReadBearer(HttpRequest req, string cookieName)
    {
        var auth = AuthHeader(req, "authorization");
        if (auth is not null && auth.StartsWith("bearer ", StringComparison.OrdinalIgnoreCase)) return AuthJs.Trim(auth[7..]);
        return AuthCookie(req, cookieName);
    }

    /// <summary>Query parameters (no '+' handling; malformed escapes are skipped), as the TS helper.</summary>
    public static Dictionary<string, string> AuthParseQueryString(string uri)
    {
        var o = new Dictionary<string, string>();
        var q = uri.IndexOf('?');
        if (q < 0) return o;
        foreach (var pair in uri[(q + 1)..].Split('&'))
        {
            var eq = pair.IndexOf('=');
            var k = AuthJs.DecodeUriComponent(eq < 0 ? pair : pair[..eq]);
            var v = eq < 0 ? "" : AuthJs.DecodeUriComponent(pair[(eq + 1)..]);
            // Like assigning to a JS object: a repeated key keeps its first position.
            if (k is not null && v is not null) o[k] = v;
        }
        return o;
    }

    static string? AuthQuery(HttpRequest req, string name) => AuthParseQueryString(req.Uri).GetValueOrDefault(name);

    static HttpResponse AuthResponse(ushort status, IEnumerable<(string, string)> headers, byte[] body) =>
        new(status, HttpVersion.Http11, headers.Select(h => new HttpHeader(h.Item1, Encoding.UTF8.GetBytes(h.Item2))).ToList(), new HttpBody(body));

    public static HttpResponse AuthJsonResponse(string json, ushort status = 200, IEnumerable<string>? cookies = null) =>
        AuthResponse(status, new[] { ("content-type", "application/json") }.Concat((cookies ?? []).Select(c => ("set-cookie", c))), Encoding.UTF8.GetBytes(json));

    public static HttpResponse AuthErrorResponse(string code, ushort status, IEnumerable<(string, string)>? extra = null) =>
        AuthResponse(status, new[] { ("content-type", "application/json") }.Concat(extra ?? []), Encoding.UTF8.GetBytes($"{{\"error\":{AuthJs.Str(code)}}}"));

    public static HttpResponse AuthRedirectResponse(string location, IEnumerable<string>? cookies = null) =>
        AuthResponse(302, new[] { ("location", location) }.Concat((cookies ?? []).Select(c => ("set-cookie", c))), []);

    static AuthConfig AuthRequireConfig(AuthTx tx) => tx.Db.AuthConfig.Singleton.Find(true) ?? throw new AuthHttpException("config_missing", 500);

    /// <summary>Run a handler body, turning AuthHttpExceptions into JSON error responses (the TS client() boundary).</summary>
    static HttpResponse AuthRoute(Func<HttpResponse> f)
    {
        try
        {
            return f();
        }
        catch (AuthHttpException e)
        {
            return AuthErrorResponse(e.Code, e.Status);
        }
    }

    static Dictionary<string, JsonElement>? AuthBodyJson(HttpRequest req)
    {
        if (AuthJs.Parse(req.Body.ToStringUtf8Lossy()) is not { ValueKind: JsonValueKind.Object } root) return null;
        var o = new Dictionary<string, JsonElement>();
        foreach (var p in root.EnumerateObject()) o[p.Name] = p.Value;
        return o;
    }

    /// <summary>A string field: null if missing or null; throws invalid_request if not a string.</summary>
    static string? AuthStrField(Dictionary<string, JsonElement> body, string key) =>
        !body.TryGetValue(key, out var v) || v.ValueKind == JsonValueKind.Null ? null
        : v.ValueKind == JsonValueKind.String ? v.GetString() : throw new AuthHttpException("invalid_request", 400);

    static string AuthRequiredStr(Dictionary<string, JsonElement> body, string key) =>
        AuthStrField(body, key) is { Length: > 0 } s ? s : throw new AuthHttpException("invalid_request", 400);

    static string AuthNormalizeEmail(string email) => AuthJs.Trim(email.ToLowerInvariant());

    // -----------------------------------------------------------------------
    // Request trust and rate limits
    // -----------------------------------------------------------------------

    static string AuthProxyHeaderName(AuthTrustedProxyHeader h) => h switch
    {
        AuthTrustedProxyHeader.CfConnectingIp => "cf-connecting-ip",
        AuthTrustedProxyHeader.XRealIp => "x-real-ip",
        _ => "x-forwarded-for",
    };

    public static string? AuthClientKey(HttpRequest req, AuthTrustedProxyHeader? trusted)
    {
        if (trusted is not { } t || AuthHeader(req, AuthProxyHeaderName(t)) is not { Length: > 0 } value) return null;
        var first = AuthJs.Trim(value.Split(',')[0]);
        return first.Length > 0 && first.Length <= 128 ? first : null;
    }

    public static string? AuthUserAgent(HttpRequest req) =>
        AuthHeader(req, "user-agent") is { } v && AuthJs.Trim(v) is { Length: > 0 and <= 512 } t ? t : null;

    static bool AuthHasControl(string s) => s.Any(c => c <= 0x1f || c == 0x7f);

    /// <summary>An application-relative redirect path, or null if unsafe.</summary>
    public static string? AuthSafeRedirectPath(string value)
    {
        if (value.Length == 0 || value.Length > 2048 || !value.StartsWith('/') || value.StartsWith("//")) return null;
        if (value.Contains('\\') || value.Contains('#') || AuthHasControl(value)) return null;
        var d = AuthJs.DecodeUriComponent(value);
        if (d is null || !d.StartsWith('/') || d.StartsWith("//") || d.Contains('\\') || d.Contains('#') || AuthHasControl(d)) return null;
        return value;
    }

    public static readonly RateLimitPolicy AuthLimitPasswordSignup = new("auth.password.signup", 5, 3600);
    public static readonly RateLimitPolicy AuthLimitPasswordLoginIp = new("auth.password.login.ip", 30, 300);
    public static readonly RateLimitPolicy AuthLimitPasswordLoginEmail = new("auth.password.login.email", 10, 300);
    public static readonly RateLimitPolicy AuthLimitPasswordForgotIp = new("auth.password.forgot.ip", 5, 3600);
    public static readonly RateLimitPolicy AuthLimitPasswordForgotEmail = new("auth.password.forgot.email", 3, 3600);
    public static readonly RateLimitPolicy AuthLimitPasswordReset = new("auth.password.reset", 5, 900);
    public static readonly RateLimitPolicy AuthLimitOauthStart = new("auth.oauth.start", 30, 300);
    public static readonly RateLimitPolicy AuthLimitEmailVerifyRequest = new("auth.email.verify_request", 5, 3600);

    /// <summary>Spend one unit per check, each in its own transaction; the first block wins.</summary>
    static HttpResponse? AuthEnforceRateLimits(HandlerContext ctx, params (RateLimitPolicy Policy, string Actor)[] checks)
    {
        foreach (var (policy, actor) in checks)
        {
            var key = AuthJs.Trim(actor.ToLowerInvariant());
            if (key.Length > 256) key = key[..256];
            var r = ctx.WithTx(tx => policy.Consume(tx.Db, tx.Timestamp, key));
            if (!r.Allowed)
            {
                return AuthErrorResponse("rate_limited", 429,
                [
                    ("retry-after", r.RetryAfterSeconds.ToString()),
                    ("x-ratelimit-limit", r.Limit.ToString()),
                    ("x-ratelimit-remaining", r.Remaining.ToString()),
                    ("x-ratelimit-reset", AuthSeconds(r.ResetAt).ToString()),
                ]);
            }
        }
        return null;
    }

    // -----------------------------------------------------------------------
    // Sessions over HTTP
    // -----------------------------------------------------------------------

    /// <summary>What AuthIssueSession returns: the session id, signed token, expiry (s) and cookie.</summary>
    public sealed record AuthIssuedSession(string SessionId, string Token, long ExpiresAt, string Cookie);

    /// <summary>Inserts a session and returns its signed token and cookie.</summary>
    public static AuthIssuedSession AuthIssueSession(AuthTx tx, AuthConfig cfg, string userId, string? ipAddress, string? userAgent, AuthHttpOptions opts)
    {
        var sessionId = AuthNewToken(tx);
        var nowSec = AuthSeconds(tx.Timestamp);
        var ttl = (long)cfg.SessionTtlSeconds;
        tx.Db.AuthSession.Insert(new AuthSession { SessionId = sessionId, UserId = userId, ExpiresAt = AuthPlusSeconds(tx.Timestamp, ttl), IpAddress = ipAddress, UserAgent = userAgent, CreatedAt = tx.Timestamp });
        var token = AuthJwt.Sign(AuthKeypair.PrivateKeyFromPem(cfg.Es256PrivateKeyPem), AuthJwt.SessionClaims(cfg.IssuerUrl, userId, nowSec, nowSec + ttl, sessionId), cfg.KeyId);
        return new(sessionId, token, nowSec + ttl, AuthMakeCookie(cfg.CookieName, token, ttl, opts.SecureCookies));
    }

    static string AuthPublicUser(AuthUser u)
    {
        var sb = new StringBuilder($"{{\"userId\":{AuthJs.Str(u.UserId)},\"email\":{AuthJs.Str(u.Email)},\"emailVerified\":{(u.EmailVerified ? "true" : "false")}");
        if (u.Name is not null) sb.Append($",\"name\":{AuthJs.Str(u.Name)}");
        if (u.Image is not null) sb.Append($",\"image\":{AuthJs.Str(u.Image)}");
        return sb.Append('}').ToString();
    }

    static AuthAccount? AuthPasswordAccount(AuthTx tx, string userId) =>
        tx.Db.AuthAccount.UserId.Filter(userId).Where(a => a.ProviderId == "password").Select(a => (AuthAccount?)a).FirstOrDefault();

    static byte[] AuthNewSalt(AuthTx tx) => AuthSecretBytes(tx)[..16];

    /// <summary>POST { email, password, name? } -> { user: { userId, email }, token } and a session cookie.</summary>
    public static HttpResponse AuthPasswordSignup(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthRoute(() =>
    {
        var ip = AuthClientKey(req, opts.TrustedProxyHeader);
        var ua = AuthUserAgent(req);
        var body = AuthBodyJson(req) ?? throw new AuthHttpException("invalid_request", 400);
        var emailRaw = AuthRequiredStr(body, "email");
        var password = AuthRequiredStr(body, "password");
        var name = AuthStrField(body, "name");
        if (password.Length < AuthMinPasswordLen) throw new AuthHttpException("password_too_short", 400);
        if (password.Length > AuthMaxPasswordLen) throw new AuthHttpException("password_too_long", 400);
        var email = AuthNormalizeEmail(emailRaw);
        if (email.Length == 0 || email.Length > AuthMaxEmailLen) throw new AuthHttpException("invalid_email", 400);
        if (name is not null && name.Length > AuthMaxSignupNameLen) throw new AuthHttpException("name_too_long", 400);
        var checks = new List<(RateLimitPolicy, string)> { (AuthLimitPasswordSignup, $"email:{email}") };
        if (ip is not null) checks.Add((AuthLimitPasswordSignup, $"ip:{ip}"));
        if (AuthEnforceRateLimits(ctx, [.. checks]) is { } limited) return limited;
        void EmailFree(AuthTx tx)
        {
            if (tx.Db.AuthUser.Email.Find(email) is not null) throw new AuthHttpException("email_taken", 409);
        }
        var salt = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            EmailFree(tx);
            AuthRequireConfig(tx);
            return AuthNewSalt(tx);
        });
        var passwordHash = AuthCrypto.HashPassword(password, salt);
        var (userId, session) = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var cfg = AuthRequireConfig(tx);
            EmailFree(tx);
            var uid = AuthNewId(tx);
            tx.Db.AuthUser.Insert(new AuthUser { UserId = uid, Email = email, EmailVerified = false, Name = name, Image = null, CreatedAt = tx.Timestamp, UpdatedAt = tx.Timestamp });
            tx.Db.AuthAccount.Insert(new AuthAccount { AccountId = AuthNewId(tx), UserId = uid, ProviderId = "password", ProviderAccountId = email, PasswordHash = passwordHash, CreatedAt = tx.Timestamp, UpdatedAt = tx.Timestamp });
            return (uid, AuthIssueSession(tx, cfg, uid, ip, ua, opts));
        });
        return AuthJsonResponse($"{{\"user\":{{\"userId\":{AuthJs.Str(userId)},\"email\":{AuthJs.Str(email)}}},\"token\":{AuthJs.Str(session.Token)}}}", 200, [session.Cookie]);
    });

    /// <summary>POST { email, password } -> { userId, token } and a session cookie.</summary>
    public static HttpResponse AuthPasswordLogin(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthRoute(() =>
    {
        var ip = AuthClientKey(req, opts.TrustedProxyHeader);
        var ua = AuthUserAgent(req);
        var body = AuthBodyJson(req) ?? throw new AuthHttpException("invalid_request", 400);
        var emailRaw = AuthRequiredStr(body, "email");
        var password = AuthRequiredStr(body, "password");
        if (password.Length > AuthMaxPasswordLen) throw new AuthHttpException("invalid_credentials", 401);
        var email = AuthNormalizeEmail(emailRaw);
        if (email.Length == 0 || email.Length > AuthMaxEmailLen) throw new AuthHttpException("invalid_credentials", 401);
        var checks = new List<(RateLimitPolicy, string)>();
        if (ip is not null) checks.Add((AuthLimitPasswordLoginIp, $"ip:{ip}"));
        checks.Add((AuthLimitPasswordLoginEmail, $"email:{email}"));
        if (AuthEnforceRateLimits(ctx, [.. checks]) is { } limited) return limited;
        var stored = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            AuthRequireConfig(tx);
            return tx.Db.AuthUser.Email.Find(email) is { } u ? AuthPasswordAccount(tx, u.UserId) : null;
        });
        // Hash outside the transaction, and a dummy for unknown accounts, so timing doesn't reveal which emails exist.
        var valid = AuthCrypto.VerifyPassword(password, stored?.PasswordHash ?? AuthCrypto.DummyPasswordHash);
        if (stored is not { PasswordHash: not null } acct || !valid) throw new AuthHttpException("invalid_credentials", 401);
        var session = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var cfg = AuthRequireConfig(tx);
            // Reject a password that changed while it was being verified.
            if (AuthPasswordAccount(tx, acct.UserId)?.PasswordHash != acct.PasswordHash) throw new AuthHttpException("invalid_credentials", 401);
            return AuthIssueSession(tx, cfg, acct.UserId, ip, ua, opts);
        });
        return AuthJsonResponse($"{{\"userId\":{AuthJs.Str(acct.UserId)},\"token\":{AuthJs.Str(session.Token)}}}", 200, [session.Cookie]);
    });

    /// <summary>GET -> { user, sessionExpiresAt } for the presented session.</summary>
    public static HttpResponse AuthMe(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthRoute(() => ctx.WithTx(t =>
    {
        var tx = AuthTx.Of(t);
        var cfg = AuthRequireConfig(tx);
        var session = AuthRequestSession(tx, cfg, req) ?? throw new AuthHttpException("unauthenticated", 401);
        var user = tx.Db.AuthUser.UserId.Find(session.UserId) ?? throw new AuthHttpException("unauthenticated", 401);
        return AuthJsonResponse($"{{\"user\":{AuthPublicUser(user)},\"sessionExpiresAt\":{AuthSeconds(session.ExpiresAt)}}}");
    }));

    /// <summary>Replaces the presented session; connections bound to it move to the new one.</summary>
    public static HttpResponse AuthRefresh(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthRoute(() =>
    {
        var ip = AuthClientKey(req, opts.TrustedProxyHeader);
        var ua = AuthUserAgent(req);
        return ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var cfg = AuthRequireConfig(tx);
            var old = AuthRequestSession(tx, cfg, req) ?? throw new AuthHttpException("unauthenticated", 401);
            var user = tx.Db.AuthUser.UserId.Find(old.UserId) ?? throw new AuthHttpException("unauthenticated", 401);
            var next = AuthIssueSession(tx, cfg, user.UserId, ip, ua, opts);
            foreach (var b in tx.Db.AuthConnectionBinding.SessionId.Filter(old.SessionId).ToList())
            {
                tx.Db.AuthConnectionBinding.StdbIdentity.Update(b with { SessionId = next.SessionId });
            }
            tx.Db.AuthSession.SessionId.Delete(old.SessionId);
            return AuthJsonResponse($"{{\"user\":{AuthPublicUser(user)},\"token\":{AuthJs.Str(next.Token)},\"sessionExpiresAt\":{next.ExpiresAt}}}", 200, [next.Cookie]);
        });
    });

    /// <summary>Deletes the presented session (if any) and clears the cookie.</summary>
    public static HttpResponse AuthLogout(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthRoute(() =>
    {
        var cookieName = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var cfg = AuthRequireConfig(tx);
            if (AuthRequestSession(tx, cfg, req) is { } s) AuthDeleteSession(tx, s);
            return cfg.CookieName;
        });
        return AuthJsonResponse("{\"ok\":true}", 200, [AuthClearCookie(cookieName, opts.SecureCookies)]);
    });

    // -----------------------------------------------------------------------
    // Email verification and password reset
    // -----------------------------------------------------------------------

    static AuthMailParams AuthBuildMail(bool verify, string to, string baseUrl, string token, string? appName)
    {
        var app = appName ?? "this app";
        var enc = AuthJs.EncodeUriComponent(token);
        if (verify)
        {
            var url = $"{baseUrl}/auth/email/verify?token={enc}";
            return new(to, $"Verify your email for {app}", $"Click to verify your email:\n\n{url}\n\nThis link expires in 24 hours.",
                $"<p>Click to verify your email:</p><p><a href=\"{url}\">{url}</a></p><p>This link expires in 24 hours.</p>");
        }
        var u = $"{baseUrl}/auth/password/reset?token={enc}";
        return new(to, $"Reset your password for {app}", $"Click to reset your password:\n\n{u}\n\nThis link expires in 1 hour. Ignore this email if the request was unexpected.",
            $"<p>Click to reset your password:</p><p><a href=\"{u}\">{u}</a></p><p>This link expires in 1 hour. Ignore this email if the request was unexpected.</p>");
    }

    static AuthVerification AuthFindVerification(AuthTx tx, string token, string purpose) =>
        tx.Db.AuthVerification.Value.Find(token) is { } row && row.Purpose == purpose && AuthMicros(row.ExpiresAt) >= AuthMicros(tx.Timestamp)
            ? row : throw new AuthHttpException("bad_token", 400);

    /// <summary>Replaces any pending token for this purpose and returns a new one.</summary>
    static string AuthNewVerification(AuthTx tx, string email, string purpose, long ttlSeconds)
    {
        foreach (var r in tx.Db.AuthVerification.Identifier.Filter(email).Where(r => r.Purpose == purpose).ToList())
        {
            tx.Db.AuthVerification.VerificationId.Delete(r.VerificationId);
        }
        var value = AuthNewToken(tx);
        tx.Db.AuthVerification.Insert(new AuthVerification { VerificationId = AuthNewId(tx), Identifier = email, Value = value, Purpose = purpose, ExpiresAt = AuthPlusSeconds(tx.Timestamp, ttlSeconds), CreatedAt = tx.Timestamp });
        return value;
    }

    /// <summary>POST { email }: mails a reset link if the account exists. Always 200.</summary>
    public static HttpResponse AuthForgotPassword(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthRoute(() =>
    {
        var sendMail = opts.SendMail ?? throw new AuthHttpException("mailer_not_configured", 500);
        var ip = AuthClientKey(req, opts.TrustedProxyHeader);
        var body = AuthBodyJson(req) ?? throw new AuthHttpException("invalid_request", 400);
        var email = AuthNormalizeEmail(AuthRequiredStr(body, "email"));
        if (email.Length == 0 || email.Length > AuthMaxEmailLen) throw new AuthHttpException("invalid_request", 400);
        var checks = new List<(RateLimitPolicy, string)>();
        if (ip is not null) checks.Add((AuthLimitPasswordForgotIp, $"ip:{ip}"));
        checks.Add((AuthLimitPasswordForgotEmail, $"email:{email}"));
        if (AuthEnforceRateLimits(ctx, [.. checks]) is { } limited) return limited;
        var mail = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var cfg = AuthRequireConfig(tx);
            if (tx.Db.AuthUser.Email.Find(email) is null) return null;
            return AuthBuildMail(false, email, cfg.BaseUrl, AuthNewVerification(tx, email, "password_reset", AuthResetTokenTtl), opts.AppName);
        });
        if (mail is not null) sendMail(ctx, mail);
        return AuthJsonResponse("{\"ok\":true}");
    });

    /// <summary>POST { token, newPassword }: sets the password, verifies the address, revokes every session.</summary>
    public static HttpResponse AuthResetPassword(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthRoute(() =>
    {
        var ip = AuthClientKey(req, opts.TrustedProxyHeader);
        var body = AuthBodyJson(req) ?? throw new AuthHttpException("invalid_request", 400);
        var token = AuthRequiredStr(body, "token");
        var newPassword = AuthRequiredStr(body, "newPassword");
        if (newPassword.Length < AuthMinPasswordLen) throw new AuthHttpException("password_too_short", 400);
        if (newPassword.Length > AuthMaxPasswordLen) throw new AuthHttpException("password_too_long", 400);
        if (token.Length > AuthMaxTokenLen) throw new AuthHttpException("bad_token", 400);
        if (ip is not null && AuthEnforceRateLimits(ctx, (AuthLimitPasswordReset, $"ip:{ip}")) is { } limited) return limited;
        // Check the token before the slow hash.
        var salt = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            AuthFindVerification(tx, token, "password_reset");
            return AuthNewSalt(tx);
        });
        var passwordHash = AuthCrypto.HashPassword(newPassword, salt);
        ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var row = AuthFindVerification(tx, token, "password_reset");
            tx.Db.AuthVerification.VerificationId.Delete(row.VerificationId);
            var user = tx.Db.AuthUser.Email.Find(row.Identifier) ?? throw new AuthHttpException("bad_token", 400);
            if (AuthPasswordAccount(tx, user.UserId) is { } acct)
            {
                tx.Db.AuthAccount.AccountId.Update(acct with { PasswordHash = passwordHash, UpdatedAt = tx.Timestamp });
            }
            else
            {
                tx.Db.AuthAccount.Insert(new AuthAccount { AccountId = AuthNewId(tx), UserId = user.UserId, ProviderId = "password", ProviderAccountId = user.Email, PasswordHash = passwordHash, CreatedAt = tx.Timestamp, UpdatedAt = tx.Timestamp });
            }
            // The reset link proved control of the mailbox.
            if (!user.EmailVerified) tx.Db.AuthUser.UserId.Update(user with { EmailVerified = true, UpdatedAt = tx.Timestamp });
            foreach (var s in tx.Db.AuthSession.UserId.Filter(user.UserId).ToList()) AuthDeleteSession(tx, s);
            return 0;
        });
        return AuthJsonResponse("{\"ok\":true}");
    });

    /// <summary>POST with a session: mails a verification link (or reports alreadyVerified).</summary>
    public static HttpResponse AuthEmailVerifyRequest(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthRoute(() =>
    {
        var sendMail = opts.SendMail ?? throw new AuthHttpException("mailer_not_configured", 500);
        if (AuthClientKey(req, opts.TrustedProxyHeader) is { } ip && AuthEnforceRateLimits(ctx, (AuthLimitEmailVerifyRequest, $"ip:{ip}")) is { } limited) return limited;
        var mail = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var cfg = AuthRequireConfig(tx);
            var session = AuthRequestSession(tx, cfg, req) ?? throw new AuthHttpException("unauthenticated", 401);
            var user = tx.Db.AuthUser.UserId.Find(session.UserId) ?? throw new AuthHttpException("unauthenticated", 401);
            if (user.EmailVerified) return null;
            return AuthBuildMail(true, user.Email, cfg.BaseUrl, AuthNewVerification(tx, user.Email, "email_verify", AuthVerifyTokenTtl), opts.AppName);
        });
        if (mail is null) return AuthJsonResponse("{\"ok\":true,\"alreadyVerified\":true}");
        sendMail(ctx, mail);
        return AuthJsonResponse("{\"ok\":true}");
    });

    /// <summary>GET ?token=: marks the address verified and redirects.</summary>
    public static HttpResponse AuthEmailVerify(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthRoute(() =>
    {
        var token = AuthQuery(req, "token") is { Length: > 0 } tk ? tk : throw new AuthHttpException("missing_token", 400);
        ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var row = AuthFindVerification(tx, token, "email_verify");
            tx.Db.AuthVerification.VerificationId.Delete(row.VerificationId);
            var user = tx.Db.AuthUser.Email.Find(row.Identifier) ?? throw new AuthHttpException("bad_token", 400);
            tx.Db.AuthUser.UserId.Update(user with { EmailVerified = true, UpdatedAt = tx.Timestamp });
            return 0;
        });
        return AuthRedirectResponse(opts.EmailVerifiedRedirect ?? "/");
    });

    // -----------------------------------------------------------------------
    // OAuth
    // -----------------------------------------------------------------------

    static readonly (string, string)[] AuthGithubHeaders =
    [
        ("accept", "application/vnd.github+json"),
        ("x-github-api-version", "2022-11-28"),
        ("user-agent", "spacetimedb-auth-submodule"),
    ];

    /// <summary>Google's profile parser, for custom Google-shaped providers.</summary>
    public static OAuthProfile AuthParseGoogleProfile(JsonElement c) => new(
        AuthJs.StringOf(AuthJs.Prop(c, "sub")), AuthJs.StringOf(AuthJs.Prop(c, "email")),
        AuthJs.Prop(c, "email_verified") is { ValueKind: JsonValueKind.True }, AuthJs.StrProp(c, "name"), AuthJs.StrProp(c, "picture"));

    static OAuthProfile AuthParseGithubProfile(JsonElement u) => new(
        AuthJs.StringOf(AuthJs.Prop(u, "id")), AuthJs.StringOf(AuthJs.Prop(u, "email")), true,
        AuthJs.StrProp(u, "name") ?? AuthJs.StringOf(AuthJs.Prop(u, "login")), AuthJs.StrProp(u, "avatar_url"));

    static string AuthPickGithubEmail(JsonElement? rows)
    {
        if (rows is not { ValueKind: JsonValueKind.Array } arr) return "";
        bool Is(JsonElement r, string k) => AuthJs.Prop(r, k) is { ValueKind: JsonValueKind.True };
        var list = arr.EnumerateArray().ToList();
        var hit = list.FirstOrDefault(r => Is(r, "primary") && Is(r, "verified") && AuthJs.StrProp(r, "email") is not null);
        if (hit.ValueKind == JsonValueKind.Undefined) hit = list.FirstOrDefault(r => Is(r, "verified") && AuthJs.StrProp(r, "email") is not null);
        return hit.ValueKind == JsonValueKind.Undefined ? "" : AuthJs.StrProp(hit, "email") ?? "";
    }

    static (ushort Status, JsonElement? Json) AuthHttpGetJson(HandlerContext ctx, string url, IEnumerable<(string, string)> headers)
    {
        var res = ctx.Http.Send(new HttpRequest { Uri = url, Method = SpacetimeDB.HttpMethod.Get, Headers = headers.Select(h => new HttpHeader(h.Item1, h.Item2)).ToList() });
        return res switch
        {
            Result<HttpResponse, HttpError>.OkR(var r) => (r.StatusCode, AuthJs.Parse(r.Body.ToStringUtf8Lossy())),
            _ => (0, null),
        };
    }

    /// <summary>GitHub: /user, then a verified address from /user/emails, both under UserInfoUrl.</summary>
    public static (OAuthProfile? Profile, string? Error) AuthResolveGithubProfile(HandlerContext ctx, OAuthProviderSpec spec, string accessToken)
    {
        var userUrl = spec.UserInfoUrl ?? "https://api.github.com/user";
        var headers = AuthGithubHeaders.Append(("authorization", $"Bearer {accessToken}")).ToList();
        var (status, user) = AuthHttpGetJson(ctx, userUrl, headers);
        if (status is < 200 or >= 300) return (null, $"userinfo_failed:{status}");
        var (es, emails) = AuthHttpGetJson(ctx, $"{userUrl}/emails", headers);
        if (es is < 200 or >= 300) return (null, $"github_email_failed:{es}");
        var u = user ?? default;
        var email = AuthPickGithubEmail(emails);
        return (new OAuthProfile(AuthJs.StringOf(user is null ? null : AuthJs.Prop(u, "id")), email, email.Length > 0,
            (user is null ? null : AuthJs.StrProp(u, "name")) ?? AuthJs.StringOf(user is null ? null : AuthJs.Prop(u, "login")),
            user is null ? null : AuthJs.StrProp(u, "avatar_url")), null);
    }

    // Properties, not static fields: static initializer order across partial files is undefined,
    // so a host's `static readonly X = AuthGoogle with { ... }` would see null.
    public static OAuthProviderSpec AuthGoogle => new()
    {
        Id = "google",
        AuthorizeUrl = "https://accounts.google.com/o/oauth2/v2/auth",
        TokenUrl = "https://oauth2.googleapis.com/token",
        Scope = "openid email profile",
        ClientId = c => c.GoogleClientId ?? "",
        ClientSecret = c => c.GoogleClientSecret ?? "",
        UserInfoUrl = "https://openidconnect.googleapis.com/v1/userinfo",
        ParseProfile = AuthParseGoogleProfile,
        AuthorizeExtras = [("access_type", "offline"), ("prompt", "consent")],
    };

    public static OAuthProviderSpec AuthGithub => new()
    {
        Id = "github",
        AuthorizeUrl = "https://github.com/login/oauth/authorize",
        TokenUrl = "https://github.com/login/oauth/access_token",
        Scope = "read:user user:email",
        ClientId = c => c.GithubClientId ?? "",
        ClientSecret = c => c.GithubClientSecret ?? "",
        UserInfoUrl = "https://api.github.com/user",
        UserInfoHeaders = AuthGithubHeaders,
        ParseProfile = AuthParseGithubProfile,
        ResolveProfile = AuthResolveGithubProfile,
        UsePkce = false,
    };

    static string AuthStateCookieName(AuthConfig cfg) => $"{cfg.CookieName}_oauth_state";
    static string AuthCallbackUrl(AuthConfig cfg, OAuthProviderSpec p) => $"{cfg.BaseUrl}/auth/{p.Id}/callback";

    static string AuthFormEncode(IEnumerable<(string, string)> ps) =>
        string.Join("&", ps.Select(p => $"{AuthJs.EncodeUriComponent(p.Item1)}={AuthJs.EncodeUriComponent(p.Item2)}"));

    /// <summary>GET ?redirectTo=: stores state (and a PKCE verifier) and redirects to the provider.</summary>
    public static HttpResponse AuthOauthStart(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts, OAuthProviderSpec provider) => AuthRoute(() =>
    {
        var requested = AuthQuery(req, "redirectTo");
        var redirectTo = (requested is null ? "/" : AuthSafeRedirectPath(requested)) ?? throw new AuthHttpException("invalid_redirect", 400);
        if (AuthClientKey(req, opts.TrustedProxyHeader) is { } ip && AuthEnforceRateLimits(ctx, (AuthLimitOauthStart, $"ip:{ip}:{provider.Id}")) is { } limited) return limited;
        var (cfg, clientId, state, verifier) = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var cfg = AuthRequireConfig(tx);
            var clientId = provider.ClientId(cfg);
            if (clientId.Length == 0) throw new AuthHttpException($"provider_not_configured:{provider.Id}", 500);
            var state = AuthNewToken(tx);
            var verifier = provider.UsePkce ? AuthNewToken(tx) : "";
            tx.Db.AuthOauthState.Insert(new AuthOauthState { State = state, Provider = provider.Id, CodeVerifier = verifier, RedirectTo = redirectTo, ExpiresAt = AuthPlusSeconds(tx.Timestamp, AuthOauthStateTtl), CreatedAt = tx.Timestamp });
            return (cfg, clientId, state, verifier);
        });
        var ps = new List<(string, string)>
        {
            ("client_id", clientId),
            ("redirect_uri", AuthCallbackUrl(cfg, provider)),
            ("response_type", "code"),
            ("scope", provider.Scope),
            ("state", state),
        };
        if (provider.UsePkce)
        {
            ps.Add(("code_challenge", AuthCrypto.PkceChallenge(verifier)));
            ps.Add(("code_challenge_method", "S256"));
        }
        foreach (var (k, v) in provider.AuthorizeExtras)
        {
            var i = ps.FindIndex(p => p.Item1 == k);
            if (i >= 0) ps[i] = (k, v);
            else ps.Add((k, v));
        }
        var sep = provider.AuthorizeUrl.Contains('?') ? '&' : '?';
        return AuthRedirectResponse($"{provider.AuthorizeUrl}{sep}{AuthFormEncode(ps)}", [AuthMakeCookie(AuthStateCookieName(cfg), state, AuthOauthStateTtl, opts.SecureCookies)]);
    });

    /// <summary>GET ?code=&amp;state=: checks state (and its browser cookie), exchanges the code, finds or creates the user, issues a session, redirects.</summary>
    public static HttpResponse AuthOauthCallback(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts, OAuthProviderSpec provider) => AuthRoute(() =>
    {
        var ip = AuthClientKey(req, opts.TrustedProxyHeader);
        var ua = AuthUserAgent(req);
        var code = AuthQuery(req, "code");
        var state = AuthQuery(req, "state");
        if (string.IsNullOrEmpty(code) || string.IsNullOrEmpty(state)) throw new AuthHttpException("missing_code_or_state", 400);
        if (code.Length > AuthMaxOauthCodeLength || state.Length > AuthMaxOauthStateLength) throw new AuthHttpException("invalid_code_or_state", 400);
        var (cfg, codeVerifier, redirectTo) = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var cfg = AuthRequireConfig(tx);
            var row = tx.Db.AuthOauthState.State.Find(state);
            var cookieState = AuthCookie(req, AuthStateCookieName(cfg));
            if (row is not { } r || r.Provider != provider.Id || AuthMicros(r.ExpiresAt) < AuthMicros(tx.Timestamp) || cookieState != state)
                throw new AuthHttpException("bad_state", 400);
            tx.Db.AuthOauthState.State.Delete(r.State);
            return (cfg, r.CodeVerifier, r.RedirectTo);
        });
        var form = new List<(string, string)>
        {
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", provider.ClientId(cfg)),
            ("client_secret", provider.ClientSecret(cfg)),
            ("redirect_uri", AuthCallbackUrl(cfg, provider)),
        };
        if (codeVerifier.Length > 0) form.Add(("code_verifier", codeVerifier));
        var tokRes = ctx.Http.Send(new HttpRequest
        {
            Uri = provider.TokenUrl,
            Method = SpacetimeDB.HttpMethod.Post,
            Headers = [new HttpHeader("content-type", "application/x-www-form-urlencoded"), new HttpHeader("accept", "application/json")],
            Body = HttpBody.FromString(AuthFormEncode(form)),
        });
        if (tokRes is not Result<HttpResponse, HttpError>.OkR(var tok) || tok.StatusCode is < 200 or >= 300) throw new AuthHttpException("token_exchange_failed", 502);
        var tokens = AuthJs.Parse(tok.Body.ToStringUtf8Lossy()) ?? default;
        var accessToken = AuthJs.StrProp(tokens, "access_token");
        var refreshToken = AuthJs.StrProp(tokens, "refresh_token");
        var idToken = AuthJs.StrProp(tokens, "id_token");
        long? expiresIn = AuthJs.Prop(tokens, "expires_in") is { ValueKind: JsonValueKind.Number } ei && ei.GetDouble() is var d && d > 0 && d % 1 == 0 && d <= 9007199254740991 ? (long)d : null;
        if (accessToken is null && idToken is null) throw new AuthHttpException("no_token_in_response", 502);
        OAuthProfile profile;
        if (provider.ResolveProfile is { } resolve && accessToken is not null)
        {
            var (p, err) = resolve(ctx, provider, accessToken);
            profile = p ?? throw new AuthHttpException(err ?? "cannot_resolve_profile", 502);
        }
        else if (provider.UserInfoUrl is { } url && accessToken is not null)
        {
            var headers = new List<(string, string)> { ("authorization", $"Bearer {accessToken}"), ("accept", "application/json"), ("user-agent", "spacetimedb-auth-submodule") };
            foreach (var (k, v) in provider.UserInfoHeaders)
            {
                headers.RemoveAll(h => string.Equals(h.Item1, k, StringComparison.OrdinalIgnoreCase));
                headers.Add((k, v));
            }
            var (status, data) = AuthHttpGetJson(ctx, url, headers);
            if (status is < 200 or >= 300) throw new AuthHttpException($"userinfo_failed:{status}", 502);
            profile = provider.ParseProfile(data ?? default);
        }
        else if (provider.Oidc && idToken is not null) throw new AuthHttpException("id_token_verification_unsupported", 502);
        else throw new AuthHttpException("cannot_resolve_profile", 502);
        if (profile.Email.Length == 0 || profile.Sub.Length == 0 || profile.Email.Length > AuthMaxProfileEmailLength || profile.Sub.Length > AuthMaxProfileSubLength
            || (profile.Name?.Length ?? 0) > AuthMaxOauthProfileNameLength || (profile.Image?.Length ?? 0) > AuthMaxOauthProfileImageLength)
            throw new AuthHttpException("incomplete_profile", 502);
        var email = profile.Email.ToLowerInvariant();
        var session = ctx.WithTx(t =>
        {
            var tx = AuthTx.Of(t);
            var cfg2 = AuthRequireConfig(tx);
            Timestamp? expiresAt = expiresIn is { } e ? AuthPlusSeconds(tx.Timestamp, e) : null;
            var existing = tx.Db.AuthAccount.ProviderAccountId.Filter(profile.Sub).Where(a => a.ProviderId == provider.Id).Select(a => (AuthAccount?)a).FirstOrDefault();
            string userId;
            if (existing is { } acct)
            {
                userId = acct.UserId;
                tx.Db.AuthAccount.AccountId.Update(acct with
                {
                    AccessToken = accessToken ?? acct.AccessToken,
                    RefreshToken = refreshToken ?? acct.RefreshToken,
                    AccessTokenExpiresAt = expiresAt ?? acct.AccessTokenExpiresAt,
                    UpdatedAt = tx.Timestamp,
                });
            }
            else
            {
                if (tx.Db.AuthUser.Email.Find(email) is { } byEmail)
                {
                    // Link only when both sides proved control of the address.
                    if (!profile.EmailVerified || !byEmail.EmailVerified) throw new AuthHttpException("account_link_required", 409);
                    userId = byEmail.UserId;
                }
                else
                {
                    userId = AuthNewId(tx);
                    tx.Db.AuthUser.Insert(new AuthUser { UserId = userId, Email = email, EmailVerified = profile.EmailVerified, Name = profile.Name, Image = profile.Image, CreatedAt = tx.Timestamp, UpdatedAt = tx.Timestamp });
                }
                tx.Db.AuthAccount.Insert(new AuthAccount { AccountId = AuthNewId(tx), UserId = userId, ProviderId = provider.Id, ProviderAccountId = profile.Sub, AccessToken = accessToken, RefreshToken = refreshToken, AccessTokenExpiresAt = expiresAt, CreatedAt = tx.Timestamp, UpdatedAt = tx.Timestamp });
            }
            return AuthIssueSession(tx, cfg2, userId, ip, ua, opts);
        });
        return AuthRedirectResponse(redirectTo, [session.Cookie, AuthClearCookie(AuthStateCookieName(cfg), opts.SecureCookies)]);
    });

    public static HttpResponse AuthGoogleStart(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthOauthStart(ctx, req, opts, AuthGoogle);
    public static HttpResponse AuthGoogleCallback(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthOauthCallback(ctx, req, opts, AuthGoogle);
    public static HttpResponse AuthGithubStart(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthOauthStart(ctx, req, opts, AuthGithub);
    public static HttpResponse AuthGithubCallback(HandlerContext ctx, HttpRequest req, AuthHttpOptions opts) => AuthOauthCallback(ctx, req, opts, AuthGithub);
}
