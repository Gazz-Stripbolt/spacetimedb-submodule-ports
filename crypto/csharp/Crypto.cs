// Crypto.cs: hashing, encoding and webhook-signature helpers for C# modules.
//
// Unofficial C# port of @spacetimedb/crypto (SpacetimeDB PR #5903, commit
// ae25f1da24b1723e28acd0038e75e598f33952c3, package spacetime-crypto-ts). Licensed
// Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
// is by Tinker (Gazz-Stripbolt).
//
// Changes from the TypeScript original:
// - Managed SHA-256 / HMAC-SHA256: System.Security.Cryptography isn't available on wasi.
// - NowSeconds is required whenever a tolerance is checked (modules have no wall clock); pass
//   ctx.Timestamp in seconds. ToleranceSeconds = null means "skip the check" (TS: Infinity).
// - Results are VerifyResult { Ok, Reason } with the same "crypto.*" codes.
//
// Drop this file into your module project. It defines no tables or reducers.

#nullable enable

using System.Text;

/// <summary>Hashing, encoding and webhook-signature helpers (port of @spacetimedb/crypto).</summary>
public static class Crypto
{
    public const int SHA256_BYTES = 32;

    /// <summary>The <c>crypto.*</c> codes a verifier fails with.</summary>
    public static class Errors
    {
        public const string MissingSignature = "crypto.missing_signature";
        public const string InvalidTimestamp = "crypto.invalid_timestamp";
        public const string TimestampOutsideTolerance = "crypto.timestamp_outside_tolerance";
        public const string InvalidSecret = "crypto.invalid_secret";
        public const string SignatureMismatch = "crypto.signature_mismatch";
    }

    /// <summary>The default replay window recommended by Stripe and Svix.</summary>
    public const ulong DefaultToleranceSeconds = 300;

    /// <summary><c>Ok</c>, or not ok with one of the <see cref="Errors"/> codes in <c>Reason</c>.</summary>
    public readonly record struct VerifyResult(bool Ok, string? Reason)
    {
        public static readonly VerifyResult Verified = new(true, null);
        public static VerifyResult Failed(string reason) => new(false, reason);
    }

    public static byte[] Sha256(byte[] data) => CryptoSha256.Hash(data);

    public static byte[] HmacSha256(byte[] key, byte[] message) => CryptoSha256.Hmac(key, message);

    /// <summary>
    /// Compare without an early exit. True iff same length and identical bytes. Only
    /// constant-time for equal lengths, which is fine for fixed-size tags.
    /// </summary>
    public static bool TimingSafeEqual(byte[] a, byte[] b)
    {
        if (a.Length != b.Length) return false;
        var diff = 0;
        for (var i = 0; i < a.Length; i++) diff |= a[i] ^ b[i];
        return diff == 0;
    }

    /// <summary>Lowercase hex.</summary>
    public static string BytesToHex(byte[] bytes)
    {
        const string hex = "0123456789abcdef";
        var sb = new StringBuilder(bytes.Length * 2);
        foreach (var b in bytes) sb.Append(hex[b >> 4]).Append(hex[b & 15]);
        return sb.ToString();
    }

    /// <summary>Decode hex (either case). Throws on odd length or a non-hex character.</summary>
    public static byte[] HexToBytes(string hex)
    {
        if (hex.Length % 2 != 0) throw new FormatException($"hex string expected, got unpadded hex of length {hex.Length}");
        static int Nib(char c) => c switch
        {
            >= '0' and <= '9' => c - '0',
            >= 'a' and <= 'f' => c - 'a' + 10,
            >= 'A' and <= 'F' => c - 'A' + 10,
            _ => -1,
        };
        var out_ = new byte[hex.Length / 2];
        for (var i = 0; i < out_.Length; i++)
        {
            int hi = Nib(hex[2 * i]), lo = Nib(hex[2 * i + 1]);
            if (hi < 0 || lo < 0) throw new FormatException($"hex string expected, got non-hex character at index {2 * i}");
            out_[i] = (byte)(hi << 4 | lo);
        }
        return out_;
    }

    /// <summary>
    /// Decode standard base64, padded or unpadded. Rejects whitespace, invalid lengths,
    /// misplaced padding and non-canonical pad bits, exactly like the TS base64ToBytes.
    /// </summary>
    public static byte[] Base64ToBytes(string b64)
    {
        foreach (var ch in b64) if (char.IsWhiteSpace(ch)) throw new FormatException("base64: whitespace is not allowed");
        if (b64.Length % 4 == 1) throw new FormatException("base64: invalid length");
        var firstPad = b64.IndexOf('=');
        if (firstPad >= 0)
        {
            var padding = b64.Length - firstPad;
            var allPad = true;
            for (var i = firstPad; i < b64.Length; i++) if (b64[i] != '=') allPad = false;
            if (padding > 2 || b64.Length % 4 != 0 || !allPad) throw new FormatException("base64: invalid padding");
        }
        var s = b64;
        while (s.Length % 4 != 0) s += "=";
        static int Val(char c) => c switch
        {
            >= 'A' and <= 'Z' => c - 'A',
            >= 'a' and <= 'z' => c - 'a' + 26,
            >= '0' and <= '9' => c - '0' + 52,
            '+' => 62,
            '/' => 63,
            '=' => 0,
            _ => -1,
        };
        var pad = s.EndsWith("==") ? 2 : s.EndsWith("=") ? 1 : 0;
        var output = new byte[s.Length / 4 * 3 - pad];
        var oi = 0;
        for (var i = 0; i < s.Length; i += 4)
        {
            int c0 = Val(s[i]), c1 = Val(s[i + 1]), c2 = Val(s[i + 2]), c3 = Val(s[i + 3]);
            if (c0 < 0 || c1 < 0 || c2 < 0 || c3 < 0) throw new FormatException("base64: invalid char");
            var finalQuartet = i + 4 == s.Length;
            if (s[i] == '=' || s[i + 1] == '=' || (!finalQuartet && (s[i + 2] == '=' || s[i + 3] == '=')))
                throw new FormatException("base64: invalid padding");
            if (s[i + 2] == '=' && s[i + 3] != '=') throw new FormatException("base64: invalid padding");
            if (s[i + 2] == '=' && (c1 & 0x0f) != 0) throw new FormatException("base64: non-canonical padding");
            if (s[i + 3] == '=' && s[i + 2] != '=' && (c2 & 0x03) != 0) throw new FormatException("base64: non-canonical padding");
            var v = (c0 << 18) | (c1 << 12) | (c2 << 6) | c3;
            if (oi < output.Length) output[oi++] = (byte)((v >> 16) & 0xff);
            if (oi < output.Length) output[oi++] = (byte)((v >> 8) & 0xff);
            if (oi < output.Length) output[oi++] = (byte)(v & 0xff);
        }
        return output;
    }

    // -----------------------------------------------------------------------
    // Webhook verifiers
    // -----------------------------------------------------------------------

    /// <summary><c>Number.parseInt(value, 10)</c>: leading whitespace, optional sign, digits; trailing junk ignored.</summary>
    static long? ParseIntLikeJs(string value)
    {
        var s = value.TrimStart();
        var neg = false;
        var i = 0;
        if (s.Length > 0 && (s[0] == '-' || s[0] == '+')) { neg = s[0] == '-'; i = 1; }
        var start = i;
        long n = 0;
        while (i < s.Length && s[i] >= '0' && s[i] <= '9')
        {
            n = n > (long.MaxValue - 9) / 10 ? long.MaxValue : n * 10 + (s[i] - '0');
            i++;
        }
        if (i == start) return null;
        return neg ? -n : n;
    }

    static VerifyResult CheckTimestamp(string value, ulong? toleranceSeconds, long nowSeconds)
    {
        if (ParseIntLikeJs(value) is not { } t) return VerifyResult.Failed(Errors.InvalidTimestamp);
        if (toleranceSeconds is { } tol)
        {
            var diff = nowSeconds >= t ? (ulong)(nowSeconds - t) : (ulong)(t - nowSeconds);
            if (diff > tol) return VerifyResult.Failed(Errors.TimestampOutsideTolerance);
        }
        return VerifyResult.Verified;
    }

    /// <summary>Options for <see cref="VerifyStripeSignature"/>.</summary>
    public sealed class StripeVerifyOpts
    {
        /// <summary>The raw request body, exactly as received.</summary>
        public required string RawBody { get; init; }
        /// <summary>Value of the <c>stripe-signature</c> header.</summary>
        public required string SignatureHeader { get; init; }
        /// <summary>The endpoint's signing secret, e.g. <c>whsec_...</c>.</summary>
        public required string Secret { get; init; }
        /// <summary>Maximum age of the signed timestamp; null skips the check (tests only).</summary>
        public ulong? ToleranceSeconds { get; init; } = DefaultToleranceSeconds;
        /// <summary>Current Unix time in seconds: <c>ctx.Timestamp.MicrosecondsSinceUnixEpoch / 1_000_000</c>.</summary>
        public required long NowSeconds { get; init; }
    }

    /// <summary>Verify a Stripe v1 signature. Any v1 entry may match (key rotation).</summary>
    public static VerifyResult VerifyStripeSignature(StripeVerifyOpts opts)
    {
        string? t = null;
        var v1 = new List<string>();
        foreach (var part in opts.SignatureHeader.Split(','))
        {
            var eq = part.IndexOf('=');
            if (eq < 0) continue;
            var k = part[..eq].Trim();
            var v = part[(eq + 1)..].Trim();
            if (k == "t") t ??= v;
            else if (k == "v1") v1.Add(v);
        }
        if (string.IsNullOrEmpty(t) || v1.Count == 0) return VerifyResult.Failed(Errors.MissingSignature);
        var ts = CheckTimestamp(t, opts.ToleranceSeconds, opts.NowSeconds);
        if (!ts.Ok) return ts;
        var expected = HmacSha256(Encoding.UTF8.GetBytes(opts.Secret), Encoding.UTF8.GetBytes($"{t}.{opts.RawBody}"));
        foreach (var candidate in v1)
        {
            byte[] c;
            try { c = HexToBytes(candidate); } catch (FormatException) { continue; }
            if (TimingSafeEqual(expected, c)) return VerifyResult.Verified;
        }
        return VerifyResult.Failed(Errors.SignatureMismatch);
    }

    /// <summary>Options for <see cref="VerifySvixSignature"/> (Svix: Resend, Clerk and others).</summary>
    public sealed class SvixVerifyOpts
    {
        public required string RawBody { get; init; }
        /// <summary><c>svix-id</c> header.</summary>
        public required string SvixId { get; init; }
        /// <summary><c>svix-timestamp</c> header (Unix seconds).</summary>
        public required string SvixTimestamp { get; init; }
        /// <summary><c>svix-signature</c> header: space-separated <c>v1,&lt;base64&gt;</c> entries.</summary>
        public required string SvixSignature { get; init; }
        /// <summary>Endpoint secret, <c>whsec_&lt;base64&gt;</c>.</summary>
        public required string Secret { get; init; }
        public ulong? ToleranceSeconds { get; init; } = DefaultToleranceSeconds;
        public required long NowSeconds { get; init; }
    }

    /// <summary>Verify a Svix webhook signature.</summary>
    public static VerifyResult VerifySvixSignature(SvixVerifyOpts opts)
    {
        var ts = CheckTimestamp(opts.SvixTimestamp, opts.ToleranceSeconds, opts.NowSeconds);
        if (!ts.Ok) return ts;
        var body = opts.Secret.StartsWith("whsec_", StringComparison.Ordinal) ? opts.Secret["whsec_".Length..] : opts.Secret;
        byte[] key;
        try { key = Base64ToBytes(body); } catch (FormatException) { return VerifyResult.Failed(Errors.InvalidSecret); }
        var expected = HmacSha256(key, Encoding.UTF8.GetBytes($"{opts.SvixId}.{opts.SvixTimestamp}.{opts.RawBody}"));
        var signatures = opts.SvixSignature.Split(' ').Where(p => p.StartsWith("v1,", StringComparison.Ordinal)).ToList();
        if (signatures.Count == 0) return VerifyResult.Failed(Errors.MissingSignature);
        foreach (var part in signatures)
        {
            byte[] c;
            try { c = Base64ToBytes(part[3..]); } catch (FormatException) { continue; }
            if (TimingSafeEqual(expected, c)) return VerifyResult.Verified;
        }
        return VerifyResult.Failed(Errors.SignatureMismatch);
    }

    /// <summary>Options for <see cref="VerifyGithubSignature"/>.</summary>
    public sealed class GithubVerifyOpts
    {
        public required string RawBody { get; init; }
        /// <summary><c>x-hub-signature-256</c> header, <c>sha256=&lt;hex&gt;</c>.</summary>
        public required string SignatureHeader { get; init; }
        public required string Secret { get; init; }
    }

    /// <summary>Verify a GitHub X-Hub-Signature-256 signature.</summary>
    public static VerifyResult VerifyGithubSignature(GithubVerifyOpts opts)
    {
        const string prefix = "sha256=";
        if (!opts.SignatureHeader.StartsWith(prefix, StringComparison.Ordinal)) return VerifyResult.Failed(Errors.MissingSignature);
        byte[] candidate;
        try { candidate = HexToBytes(opts.SignatureHeader[prefix.Length..]); }
        catch (FormatException) { return VerifyResult.Failed(Errors.SignatureMismatch); }
        var expected = HmacSha256(Encoding.UTF8.GetBytes(opts.Secret), Encoding.UTF8.GetBytes(opts.RawBody));
        return TimingSafeEqual(expected, candidate) ? VerifyResult.Verified : VerifyResult.Failed(Errors.SignatureMismatch);
    }
}

/// <summary>Managed SHA-256 / HMAC-SHA256 (System.Security.Cryptography isn't available on wasi).</summary>
static class CryptoSha256
{
    static readonly uint[] K =
    {
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    };

    static uint Rotr(uint x, int n) => (x >> n) | (x << (32 - n));

    public static byte[] Hash(byte[] data)
    {
        uint[] h = { 0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19 };
        var bitLen = (ulong)data.LongLength * 8;
        var padded = new byte[((data.Length + 9 + 63) / 64) * 64];
        data.CopyTo(padded, 0);
        padded[data.Length] = 0x80;
        for (var i = 0; i < 8; i++) padded[padded.Length - 1 - i] = (byte)(bitLen >> (8 * i));
        var w = new uint[64];
        for (var chunk = 0; chunk < padded.Length; chunk += 64)
        {
            for (var i = 0; i < 16; i++)
                w[i] = (uint)(padded[chunk + 4 * i] << 24 | padded[chunk + 4 * i + 1] << 16 | padded[chunk + 4 * i + 2] << 8 | padded[chunk + 4 * i + 3]);
            for (var i = 16; i < 64; i++)
            {
                var s0 = Rotr(w[i - 15], 7) ^ Rotr(w[i - 15], 18) ^ (w[i - 15] >> 3);
                var s1 = Rotr(w[i - 2], 17) ^ Rotr(w[i - 2], 19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16] + s0 + w[i - 7] + s1;
            }
            uint a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
            for (var i = 0; i < 64; i++)
            {
                var t1 = hh + (Rotr(e, 6) ^ Rotr(e, 11) ^ Rotr(e, 25)) + ((e & f) ^ (~e & g)) + K[i] + w[i];
                var t2 = (Rotr(a, 2) ^ Rotr(a, 13) ^ Rotr(a, 22)) + ((a & b) ^ (a & c) ^ (b & c));
                hh = g; g = f; f = e; e = d + t1; d = c; c = b; b = a; a = t1 + t2;
            }
            h[0] += a; h[1] += b; h[2] += c; h[3] += d; h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
        }
        var result = new byte[32];
        for (var i = 0; i < 8; i++)
        {
            result[4 * i] = (byte)(h[i] >> 24); result[4 * i + 1] = (byte)(h[i] >> 16);
            result[4 * i + 2] = (byte)(h[i] >> 8); result[4 * i + 3] = (byte)h[i];
        }
        return result;
    }

    public static byte[] Hmac(byte[] key, byte[] message)
    {
        if (key.Length > 64) key = Hash(key);
        var k = new byte[64];
        key.CopyTo(k, 0);
        var inner = new byte[64 + message.Length];
        var outer = new byte[64 + 32];
        for (var i = 0; i < 64; i++) { inner[i] = (byte)(k[i] ^ 0x36); outer[i] = (byte)(k[i] ^ 0x5c); }
        message.CopyTo(inner, 64);
        Hash(inner).CopyTo(outer, 64);
        return Hash(outer);
    }
}
