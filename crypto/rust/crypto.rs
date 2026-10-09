//! # crypto.rs: hashing, encoding and webhook-signature helpers for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/crypto` (SpacetimeDB PR #5903, commit
//! `ae25f1da24b1723e28acd0038e75e598f33952c3`, package `spacetime-crypto-ts`).
//! Licensed Apache-2.0, like the original. Copyright Clockwork Labs for the original
//! design and behaviour; this port is by Tinker (Gazz-Stripbolt).
//!
//! Changes from the TypeScript original:
//! - `now_seconds` is required whenever a tolerance is checked. Modules have no wall clock,
//!   so pass `ctx.timestamp` in seconds (the TS version falls back to `Date.now()`).
//! - `tolerance_seconds: None` means "skip the check" (the TS version uses `Infinity`).
//! - Results are `Result<(), VerifyFailure>`; `VerifyFailure::code()` gives the same
//!   `crypto.*` strings as the TS `errors` object.
//! - SHA-256 and HMAC come from the `sha2` and `hmac` crates instead of `@noble/hashes`.
//!
//! Copy this file to `src/crypto.rs`, add `pub mod crypto;` to `lib.rs`, and add
//! `sha2 = "0.10"` and `hmac = "0.12"` to `Cargo.toml`. It defines no tables or reducers.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Length of a SHA-256 digest in bytes.
pub const SHA256_BYTES: usize = 32;

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> [u8; SHA256_BYTES] {
    Sha256::digest(data).into()
}

/// HMAC-SHA256 of `message` under `key`.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; SHA256_BYTES] {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// Compare two byte slices without an early exit. Returns true iff both have the same length
/// and identical bytes. Only constant-time for equal lengths (the length check leaks), which is
/// fine for fixed-size tags.
pub fn timing_safe_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Lowercase hex.
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

/// Decode hex (either case). Errors on odd length or a non-hex character.
pub fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    let h = hex.as_bytes();
    if !h.len().is_multiple_of(2) {
        return Err(format!(
            "hex string expected, got unpadded hex of length {}",
            h.len()
        ));
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    h.chunks(2)
        .enumerate()
        .map(|(i, p)| match (nib(p[0]), nib(p[1])) {
            (Some(a), Some(b)) => Ok(a << 4 | b),
            _ => Err(format!(
                "hex string expected, got non-hex character at index {}",
                i * 2
            )),
        })
        .collect()
}

/// Decode standard base64, padded or unpadded. Rejects whitespace, invalid lengths, misplaced
/// padding and non-canonical pad bits, exactly like the TS `base64ToBytes`.
pub fn base64_to_bytes(b64: &str) -> Result<Vec<u8>, String> {
    if b64.chars().any(char::is_whitespace) {
        return Err("base64: whitespace is not allowed".into());
    }
    // Lengths and indices are in UTF-16 code units in the original; any non-ASCII char is
    // invalid anyway, so count chars.
    let chars: Vec<char> = b64.chars().collect();
    let len = chars.len();
    if len % 4 == 1 {
        return Err("base64: invalid length".into());
    }
    if let Some(first_pad) = chars.iter().position(|&c| c == '=') {
        let padding = len - first_pad;
        if padding > 2 || !len.is_multiple_of(4) || chars[first_pad..].iter().any(|&c| c != '=') {
            return Err("base64: invalid padding".into());
        }
    }
    let mut s = chars;
    while !s.len().is_multiple_of(4) {
        s.push('=');
    }
    let val = |c: char| -> i32 {
        match c {
            'A'..='Z' => c as i32 - 'A' as i32,
            'a'..='z' => c as i32 - 'a' as i32 + 26,
            '0'..='9' => c as i32 - '0' as i32 + 52,
            '+' => 62,
            '/' => 63,
            '=' => 0,
            _ => -1,
        }
    };
    let pad = if s.ends_with(&['=', '=']) {
        2
    } else if s.last() == Some(&'=') {
        1
    } else {
        0
    };
    let out_len = s.len() / 4 * 3 - pad;
    let mut out = Vec::with_capacity(out_len);
    for (q, quad) in s.chunks(4).enumerate() {
        let c: Vec<i32> = quad.iter().map(|&ch| val(ch)).collect();
        if c.iter().any(|&v| v < 0) {
            return Err("base64: invalid char".into());
        }
        let final_quartet = (q + 1) * 4 == s.len();
        if quad[0] == '='
            || quad[1] == '='
            || (!final_quartet && (quad[2] == '=' || quad[3] == '='))
        {
            return Err("base64: invalid padding".into());
        }
        if quad[2] == '=' && quad[3] != '=' {
            return Err("base64: invalid padding".into());
        }
        if quad[2] == '=' && (c[1] & 0x0f) != 0 {
            return Err("base64: non-canonical padding".into());
        }
        if quad[3] == '=' && quad[2] != '=' && (c[2] & 0x03) != 0 {
            return Err("base64: non-canonical padding".into());
        }
        let v = (c[0] << 18) | (c[1] << 12) | (c[2] << 6) | c[3];
        for b in [(v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff] {
            if out.len() < out_len {
                out.push(b as u8);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Webhook verifiers
// ---------------------------------------------------------------------------

/// Reasons a webhook signature check fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyFailure {
    MissingSignature,
    InvalidTimestamp,
    TimestampOutsideTolerance,
    InvalidSecret,
    SignatureMismatch,
}

impl VerifyFailure {
    /// The same code strings as the TS `errors` object.
    pub fn code(self) -> &'static str {
        match self {
            Self::MissingSignature => errors::MISSING_SIGNATURE,
            Self::InvalidTimestamp => errors::INVALID_TIMESTAMP,
            Self::TimestampOutsideTolerance => errors::TIMESTAMP_OUTSIDE_TOLERANCE,
            Self::InvalidSecret => errors::INVALID_SECRET,
            Self::SignatureMismatch => errors::SIGNATURE_MISMATCH,
        }
    }
}

impl std::fmt::Display for VerifyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// `Ok(())` when the signature verifies.
pub type VerifyResult = Result<(), VerifyFailure>;

/// The `crypto.*` codes.
pub mod errors {
    pub const MISSING_SIGNATURE: &str = "crypto.missing_signature";
    pub const INVALID_TIMESTAMP: &str = "crypto.invalid_timestamp";
    pub const TIMESTAMP_OUTSIDE_TOLERANCE: &str = "crypto.timestamp_outside_tolerance";
    pub const INVALID_SECRET: &str = "crypto.invalid_secret";
    pub const SIGNATURE_MISMATCH: &str = "crypto.signature_mismatch";
}

/// The default replay window recommended by Stripe and Svix.
pub const DEFAULT_TOLERANCE_SECONDS: u64 = 300;

/// `Number.parseInt(value, 10)`: leading whitespace, optional sign, then digits; trailing junk
/// is ignored. `None` when there are no digits.
fn parse_int_like_js(value: &str) -> Option<i64> {
    let s = value.trim_start();
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    if end == 0 {
        return None;
    }
    let mut n: i64 = 0;
    for b in digits[..end].bytes() {
        n = n.saturating_mul(10).saturating_add((b - b'0') as i64);
    }
    Some(if neg { -n } else { n })
}

fn check_timestamp(value: &str, tolerance_seconds: Option<u64>, now_seconds: i64) -> VerifyResult {
    let t = parse_int_like_js(value).ok_or(VerifyFailure::InvalidTimestamp)?;
    if let Some(tol) = tolerance_seconds
        && now_seconds.abs_diff(t) > tol
    {
        return Err(VerifyFailure::TimestampOutsideTolerance);
    }
    Ok(())
}

/// Options for [`verify_stripe_signature`].
pub struct StripeVerifyOpts<'a> {
    /// The raw request body, exactly as received (don't re-serialize JSON).
    pub raw_body: &'a str,
    /// Value of the `stripe-signature` header.
    pub signature_header: &'a str,
    /// The endpoint's signing secret, e.g. `whsec_...`.
    pub secret: &'a str,
    /// Maximum age of the signed timestamp. `None` skips the check (tests only).
    pub tolerance_seconds: Option<u64>,
    /// Current Unix time in seconds: `ctx.timestamp.to_micros_since_unix_epoch() / 1_000_000`.
    pub now_seconds: i64,
}

/// Verify a Stripe `v1` signature. Any `v1` entry in the header may match (key rotation).
pub fn verify_stripe_signature(opts: &StripeVerifyOpts) -> VerifyResult {
    let mut t: Option<&str> = None;
    let mut v1: Vec<&str> = Vec::new();
    for part in opts.signature_header.split(',') {
        let Some(eq) = part.find('=') else { continue };
        let (k, v) = (part[..eq].trim(), part[eq + 1..].trim());
        match k {
            "t" if t.is_none() => t = Some(v),
            "v1" => v1.push(v),
            _ => {}
        }
    }
    let (Some(t), false) = (t.filter(|s| !s.is_empty()), v1.is_empty()) else {
        return Err(VerifyFailure::MissingSignature);
    };
    check_timestamp(t, opts.tolerance_seconds, opts.now_seconds)?;
    let expected = hmac_sha256(
        opts.secret.as_bytes(),
        format!("{t}.{}", opts.raw_body).as_bytes(),
    );
    for candidate in v1 {
        if let Ok(c) = hex_to_bytes(candidate)
            && timing_safe_equal(&expected, &c)
        {
            return Ok(());
        }
    }
    Err(VerifyFailure::SignatureMismatch)
}

/// Options for [`verify_svix_signature`] (Svix, as sent by Resend, Clerk and others).
pub struct SvixVerifyOpts<'a> {
    pub raw_body: &'a str,
    /// `svix-id` header.
    pub svix_id: &'a str,
    /// `svix-timestamp` header (Unix seconds).
    pub svix_timestamp: &'a str,
    /// `svix-signature` header: space-separated `v1,<base64>` entries.
    pub svix_signature: &'a str,
    /// Endpoint secret, `whsec_<base64>`.
    pub secret: &'a str,
    pub tolerance_seconds: Option<u64>,
    pub now_seconds: i64,
}

/// Verify a Svix webhook signature.
pub fn verify_svix_signature(opts: &SvixVerifyOpts) -> VerifyResult {
    check_timestamp(
        opts.svix_timestamp,
        opts.tolerance_seconds,
        opts.now_seconds,
    )?;
    let body = opts.secret.strip_prefix("whsec_").unwrap_or(opts.secret);
    let key = base64_to_bytes(body).map_err(|_| VerifyFailure::InvalidSecret)?;
    let expected = hmac_sha256(
        &key,
        format!("{}.{}.{}", opts.svix_id, opts.svix_timestamp, opts.raw_body).as_bytes(),
    );
    let signatures: Vec<&str> = opts
        .svix_signature
        .split(' ')
        .filter(|p| p.starts_with("v1,"))
        .collect();
    if signatures.is_empty() {
        return Err(VerifyFailure::MissingSignature);
    }
    for part in signatures {
        if let Ok(c) = base64_to_bytes(&part[3..])
            && timing_safe_equal(&expected, &c)
        {
            return Ok(());
        }
    }
    Err(VerifyFailure::SignatureMismatch)
}

/// Options for [`verify_github_signature`].
pub struct GithubVerifyOpts<'a> {
    pub raw_body: &'a str,
    /// `x-hub-signature-256` header, `sha256=<hex>`.
    pub signature_header: &'a str,
    pub secret: &'a str,
}

/// Verify a GitHub `X-Hub-Signature-256` signature.
pub fn verify_github_signature(opts: &GithubVerifyOpts) -> VerifyResult {
    let Some(hex) = opts.signature_header.strip_prefix("sha256=") else {
        return Err(VerifyFailure::MissingSignature);
    };
    let candidate = hex_to_bytes(hex).map_err(|_| VerifyFailure::SignatureMismatch)?;
    let expected = hmac_sha256(opts.secret.as_bytes(), opts.raw_body.as_bytes());
    if timing_safe_equal(&expected, &candidate) {
        Ok(())
    } else {
        Err(VerifyFailure::SignatureMismatch)
    }
}
