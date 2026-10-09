# crypto: hashing, encoding and webhook signatures

Port of [`@spacetimedb/crypto`](https://github.com/clockworklabs/SpacetimeDB/pull/5903) at
`ae25f1da24b1723e28acd0038e75e598f33952c3`. No tables or reducers, just functions.

- **Rust:** copy [`rust/crypto.rs`](rust/crypto.rs) and add `sha2 = "0.10"` and `hmac = "0.12"` to `Cargo.toml`.
- **C#:** add [`csharp/Crypto.cs`](csharp/Crypto.cs). It has its own SHA-256, because `System.Security.Cryptography`
  isn't available on wasi.

| TypeScript | Rust | C# |
|---|---|---|
| `sha256(data)` | `sha256(&[u8]) -> [u8; 32]` | `Crypto.Sha256(byte[])` |
| `hmacSha256(key, msg)` | `hmac_sha256(&[u8], &[u8])` | `Crypto.HmacSha256(byte[], byte[])` |
| `timingSafeEqual(a, b)` | `timing_safe_equal` | `Crypto.TimingSafeEqual` |
| `hexToBytes` / `bytesToHex` | `hex_to_bytes -> Result` / `bytes_to_hex` | `Crypto.HexToBytes` (throws `FormatException`) / `Crypto.BytesToHex` |
| `base64ToBytes` (strict) | `base64_to_bytes -> Result` | `Crypto.Base64ToBytes` (throws `FormatException`) |
| `verifyStripeSignature(opts)` | `verify_stripe_signature(&StripeVerifyOpts)` | `Crypto.VerifyStripeSignature(new() { ... })` |
| `verifySvixSignature(opts)` | `verify_svix_signature(&SvixVerifyOpts)` | `Crypto.VerifySvixSignature(new() { ... })` |
| `verifyGithubSignature(opts)` | `verify_github_signature(&GithubVerifyOpts)` | `Crypto.VerifyGithubSignature(new() { ... })` |
| `{ ok: true } \| { ok: false, reason }` | `Result<(), VerifyFailure>`, `.code()` | `VerifyResult(Ok, Reason)` |
| `errors.*` | `errors::*` | `Crypto.Errors.*` |

```rust
let r = crypto::verify_stripe_signature(&crypto::StripeVerifyOpts {
    raw_body: &body,
    signature_header: req.headers().get("stripe-signature").and_then(|v| v.to_str().ok()).unwrap_or(""),
    secret: &secret,
    tolerance_seconds: Some(crypto::DEFAULT_TOLERANCE_SECONDS),
    now_seconds: ctx.timestamp.to_micros_since_unix_epoch() / 1_000_000,
});
```

**Differences:** `now_seconds` / `NowSeconds` is required, because modules have no wall clock (the TS version falls
back to `Date.now()`). A tolerance of `None` / `null` skips the timestamp check (TS: `Infinity`).

**Tests:** the upstream vectors (NIST and RFC 6234 SHA-256, RFC 4231 HMAC, RFC 4648 base64 plus every malformed
case, and Stripe, Svix and GitHub round trips) in [`tests/crypto.test.mjs`](../tests/crypto.test.mjs), run against
[`demo/rust`](demo/rust) and [`demo/csharp`](demo/csharp).
