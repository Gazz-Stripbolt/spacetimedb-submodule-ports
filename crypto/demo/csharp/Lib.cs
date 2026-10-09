// Demo for Crypto.cs: each helper behind a procedure, so the shared test suite can drive it
// over HTTP and compare the Rust and C# ports byte for byte. The C# twin of demo/rust.

#pragma warning disable STDB_UNSTABLE

using System.Text;
using SpacetimeDB;

public static partial class Module
{
    static Result<string, string> CryptoTry(Func<string> f)
    {
        try { return Result<string, string>.Ok(f()); }
        catch (FormatException e) { return Result<string, string>.Err(e.Message); }
    }

    /// <summary>SHA-256 of hex input, as hex. <c>repeat</c> repeats the input (for the 1,000,000 × "a" vector).</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> CryptoSha256(ProcedureContext ctx, string dataHex, uint repeat) => CryptoTry(() =>
    {
        var one = Crypto.HexToBytes(dataHex);
        var n = (int)Math.Max(1, repeat);
        var data = new byte[one.Length * n];
        for (var i = 0; i < n; i++) one.CopyTo(data, i * one.Length);
        return Crypto.BytesToHex(Crypto.Sha256(data));
    });

    [SpacetimeDB.Procedure]
    public static Result<string, string> CryptoHmacSha256(ProcedureContext ctx, string keyHex, string messageHex) =>
        CryptoTry(() => Crypto.BytesToHex(Crypto.HmacSha256(Crypto.HexToBytes(keyHex), Crypto.HexToBytes(messageHex))));

    [SpacetimeDB.Procedure]
    public static Result<bool, string> CryptoTimingSafeEqual(ProcedureContext ctx, string aHex, string bHex)
    {
        try { return Result<bool, string>.Ok(Crypto.TimingSafeEqual(Crypto.HexToBytes(aHex), Crypto.HexToBytes(bHex))); }
        catch (FormatException e) { return Result<bool, string>.Err(e.Message); }
    }

    /// <summary>Decode base64 to hex, or the error message.</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> CryptoBase64ToHex(ProcedureContext ctx, string b64) =>
        CryptoTry(() => Crypto.BytesToHex(Crypto.Base64ToBytes(b64)));

    /// <summary>Decode hex and re-encode it (lowercase), or the error message.</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> CryptoHexRoundtrip(ProcedureContext ctx, string hexIn) =>
        CryptoTry(() => Crypto.BytesToHex(Crypto.HexToBytes(hexIn)));

    static string CryptoOutcome(Crypto.VerifyResult r) => r.Ok ? "ok" : r.Reason!;

    [SpacetimeDB.Procedure]
    public static string CryptoVerifyStripe(ProcedureContext ctx, string rawBody, string signatureHeader, string secret, ulong? toleranceSeconds, long nowSeconds) =>
        CryptoOutcome(Crypto.VerifyStripeSignature(new()
        {
            RawBody = rawBody,
            SignatureHeader = signatureHeader,
            Secret = secret,
            ToleranceSeconds = toleranceSeconds,
            NowSeconds = nowSeconds,
        }));

    [SpacetimeDB.Procedure]
    public static string CryptoVerifySvix(ProcedureContext ctx, string rawBody, string svixId, string svixTimestamp, string svixSignature, string secret, ulong? toleranceSeconds, long nowSeconds) =>
        CryptoOutcome(Crypto.VerifySvixSignature(new()
        {
            RawBody = rawBody,
            SvixId = svixId,
            SvixTimestamp = svixTimestamp,
            SvixSignature = svixSignature,
            Secret = secret,
            ToleranceSeconds = toleranceSeconds,
            NowSeconds = nowSeconds,
        }));

    [SpacetimeDB.Procedure]
    public static string CryptoVerifyGithub(ProcedureContext ctx, string rawBody, string signatureHeader, string secret) =>
        CryptoOutcome(Crypto.VerifyGithubSignature(new() { RawBody = rawBody, SignatureHeader = signatureHeader, Secret = secret }));
}
