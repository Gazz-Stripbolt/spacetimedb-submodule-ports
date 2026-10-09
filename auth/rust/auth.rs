//! # auth.rs: accounts, sign-in and sessions for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/auth` (SpacetimeDB PR #5905, commit
//! `37ee2ad627568f95df8229647cff5a482d629c25`, package `spacetime-auth-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Same tables, error codes, password hashes (`scrypt$N$r$p$salt$hash`), ES256 session JWTs,
//! PEM encodings, key ids, derived secrets and HTTP behaviour as the TS package: given the
//! same signing key, counter and timestamp, every language mints the same tokens.
//!
//! Needs the crypto and rate-limit ports next to it (`crate::crypto`, `crate::rate_limit`)
//! and these crates (pure Rust, build for wasm32):
//!
//! ```toml
//! spacetimedb = { version = "2.11.*", features = ["unstable"] }
//! http = "1"
//! p256 = { version = "0.13", default-features = false, features = ["ecdsa", "arithmetic"] }
//! scrypt = { version = "0.11", default-features = false }
//! serde_json = { version = "1", features = ["preserve_order"] }
//! ```
//!
//! ```ignore
//! #[spacetimedb::reducer(init)]
//! pub fn init(ctx: &ReducerContext) { auth::install(ctx); }
//!
//! const AUTH_HTTP: auth::AuthHttpOptions = auth::AuthHttpOptions { app_name: Some("Notes"), ..auth::AuthHttpOptions::DEFAULT };
//!
//! #[spacetimedb::http::handler]
//! fn auth_password_signup(ctx: &mut HandlerContext, req: Request) -> Response { auth::password_signup(ctx, req, &AUTH_HTTP) }
//! // ... one handler per route, then a #[router] with .post("/auth/password/signup", auth_password_signup) etc.
//!
//! #[spacetimedb::reducer]
//! pub fn create_note(ctx: &ReducerContext, title: String) -> Result<(), String> {
//!     let user_id = auth::require_caller_user_id(ctx)?;
//!     // ...
//! }
//! ```
//!
//! Changes from the TypeScript original:
//! - The generic operation names carry the package: `link_connection` → `auth_link_connection`,
//!   `unlink_connection`, `update_profile`, `revoke_session`, `list_my_sessions`,
//!   `revoke_my_session` and `whoami` likewise. `set_auth_config`, `add_auth_admin`,
//!   `remove_auth_admin`, `get_auth_public_key`, `auth_sweep` and `my_auth_user` are unchanged.
//! - Procedures return `Result<T, String>`; reducers return `Err(code)` where TS throws.
//! - HTTP handlers are plain functions taking `&AuthHttpOptions`; the host registers them.
//!   OAuth providers are `OAuthProviderSpec` values; the GitHub resolver reads the user and
//!   emails endpoints relative to the spec's `user_info_url` (the built-in spec points at
//!   `https://api.github.com/user`, as in TS), so a test double can stand in for it.
//! - Request bodies with non-string `email`/`password`/`name` fields are `invalid_request`
//!   (TS throws a TypeError, a 500). Unparseable token or userinfo responses are 502s with
//!   the nearest TS code (TS throws, a 500).
//! - `verify_password` returns false for hashes whose scrypt parameters are invalid or need
//!   more than 1 GiB (noble's limit); TS throws.
//! - `auth_sweep` rejects callers other than the scheduler.

use crate::crypto;
use crate::rate_limit::{self, RateLimitPolicy, RateLimitResult};
use http::header::HeaderValue;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use serde_json::{Map, Value};
use spacetimedb::http::{Body, HandlerContext, Request, Response};
use spacetimedb::{
    Identity, ProcedureContext, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration,
    Timestamp, TxContext, ViewContext,
};

const ONE_SECOND_MICROS: i64 = 1_000_000;
const DEFAULT_COOKIE_NAME: &str = "stdb_auth";
const DEFAULT_SESSION_TTL_SECONDS: u64 = 60 * 60 * 24 * 7;
const MAX_PROFILE_NAME_LEN: usize = 64;
const MAX_PROFILE_IMAGE_LEN: usize = 2048;
pub const MIN_PASSWORD_LEN: usize = 8;
pub const MAX_PASSWORD_LEN: usize = 1024;
pub const MAX_EMAIL_LEN: usize = 320;
const MAX_SIGNUP_NAME_LEN: usize = 128;
const MAX_TOKEN_LEN: usize = 256;
const RESET_TOKEN_TTL_SECONDS: i64 = 60 * 60;
const VERIFY_TOKEN_TTL_SECONDS: i64 = 60 * 60 * 24;
const OAUTH_STATE_TTL_SECONDS: i64 = 600;
const MAX_OAUTH_CODE_LENGTH: usize = 4096;
const MAX_OAUTH_STATE_LENGTH: usize = 256;
const MAX_PROFILE_SUB_LENGTH: usize = 512;
const MAX_PROFILE_EMAIL_LENGTH: usize = 320;
const MAX_OAUTH_PROFILE_NAME_LENGTH: usize = 256;
const MAX_OAUTH_PROFILE_IMAGE_LENGTH: usize = 2048;
// N=2^14 as in TS.
const SCRYPT_N: u64 = 1 << 14;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
const SCRYPT_DK_LEN: usize = 32;

/// The `auth.*` codes reducers and procedures fail with.
pub mod errors {
    pub const NOT_AUTHENTICATED: &str = "auth.not_authenticated";
    pub const NOT_AUTHORIZED: &str = "auth.not_authorized";
    pub const CONFIG_MISSING: &str = "auth.config_missing";
    pub const SIGNING_KEY_REQUIRED: &str = "auth.signing_key_required";
    pub const INVALID_PRIVATE_KEY_PEM: &str = "auth.invalid_private_key_pem";
    pub const INVALID_TOKEN: &str = "auth.invalid_token";
    pub const SESSION_NOT_FOUND: &str = "auth.session_not_found";
    pub const SESSION_NOT_OWNED: &str = "auth.session_not_owned";
    pub const USER_NOT_FOUND: &str = "auth.user_not_found";
    pub const NAME_TOO_LONG: &str = "auth.name_too_long";
    pub const IMAGE_TOO_LONG: &str = "auth.image_too_long";
    pub const LAST_ADMIN: &str = "auth.last_admin";
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

#[spacetimedb::table(accessor = auth_user)]
#[derive(Clone, Debug)]
pub struct AuthUser {
    #[primary_key]
    pub user_id: String,
    #[unique]
    pub email: String,
    pub email_verified: bool,
    pub name: Option<String>,
    pub image: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = auth_session)]
#[derive(Clone, Debug)]
pub struct AuthSession {
    #[primary_key]
    pub session_id: String,
    #[index(btree)]
    pub user_id: String,
    #[index(btree)]
    pub expires_at: Timestamp,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: Timestamp,
}

/// `provider_id`: `password`, `google`, `github`, ...; `provider_account_id`: email or provider sub.
#[spacetimedb::table(accessor = auth_account)]
#[derive(Clone, Debug)]
pub struct AuthAccount {
    #[primary_key]
    pub account_id: String,
    #[index(btree)]
    pub user_id: String,
    #[index(btree)]
    pub provider_id: String,
    #[index(btree)]
    pub provider_account_id: String,
    pub password_hash: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub access_token_expires_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = auth_verification)]
#[derive(Clone, Debug)]
pub struct AuthVerification {
    #[primary_key]
    pub verification_id: String,
    #[index(btree)]
    pub identifier: String,
    #[unique]
    pub value: String,
    pub purpose: String,
    #[index(btree)]
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
}

#[spacetimedb::table(accessor = auth_oauth_state)]
#[derive(Clone, Debug)]
pub struct AuthOauthState {
    #[primary_key]
    pub state: String,
    pub provider: String,
    pub code_verifier: String,
    pub redirect_to: String,
    #[index(btree)]
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
}

/// Private singleton, populated by `set_auth_config`.
#[spacetimedb::table(accessor = auth_config)]
#[derive(Clone, Debug)]
pub struct AuthConfig {
    #[primary_key]
    pub singleton: bool,
    pub issuer_url: String,
    pub base_url: String,
    pub cookie_name: String,
    pub session_ttl_seconds: u64,
    pub es256_private_key_pem: String,
    pub es256_public_key_pem: String,
    pub key_id: String,
    /// Advances on every secret draw (see [`secret_bytes`]).
    pub token_counter: u64,
    pub google_client_id: Option<String>,
    pub google_client_secret: Option<String>,
    pub github_client_id: Option<String>,
    pub github_client_secret: Option<String>,
    pub updated_at: Timestamp,
}

/// Maps a SpacetimeDB identity to the session it presented to `auth_link_connection`.
#[spacetimedb::table(accessor = auth_connection_binding)]
#[derive(Clone, Debug)]
pub struct AuthConnectionBinding {
    #[primary_key]
    pub stdb_identity: Identity,
    #[index(btree)]
    pub user_id: String,
    #[index(btree)]
    pub session_id: String,
    pub linked_at: Timestamp,
}

/// Operator allowlist; `install` seeds the publishing identity.
#[spacetimedb::table(accessor = auth_admin_identity)]
#[derive(Clone, Debug)]
pub struct AuthAdminIdentity {
    #[primary_key]
    pub identity: Identity,
    pub added_at: Timestamp,
}

#[spacetimedb::table(accessor = auth_sweeper_tick, scheduled(auth_sweep))]
pub struct AuthSweeperTick {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct AuthPubKey {
    pub public_key_pem: String,
    pub key_id: String,
    pub issuer_url: String,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct MySession {
    pub session_id: String,
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub is_current: bool,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct MySessions {
    pub sessions: Vec<MySession>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct WhoAmI {
    pub user_id: Option<String>,
    pub sender_identity_hex: String,
}

// ---------------------------------------------------------------------------
// Small helpers (JS-compatible lengths and JSON)
// ---------------------------------------------------------------------------

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s.slice(0, n)` in UTF-16 code units (a split surrogate pair is dropped, not mangled).
fn utf16_prefix(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.len_utf16();
        if used + w > n {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

/// `JSON.stringify` of a string.
pub fn js_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn now_micros(ts: Timestamp) -> i64 {
    ts.to_micros_since_unix_epoch()
}

fn plus_seconds(ts: Timestamp, secs: i64) -> Timestamp {
    ts + TimeDuration::from_micros(secs * ONE_SECOND_MICROS)
}

// ---------------------------------------------------------------------------
// Crypto: base64, scrypt, secrets, ids, PKCE
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        out.push(B64[(b[0] >> 2) as usize] as char);
        out.push(B64[(((b[0] & 3) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64[(((b[1] & 15) << 2) | (b[2] >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64[(b[2] & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Base64url without padding.
pub fn base64url_encode(bytes: &[u8]) -> String {
    base64_encode(bytes)
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string()
}

/// Base64url decode; rejects `+`, `/` and `=` like TS.
pub fn base64url_decode(value: &str) -> Result<Vec<u8>, String> {
    if value.contains(['+', '/', '=']) {
        return Err("base64url: invalid char".into());
    }
    crypto::base64_to_bytes(&value.replace('-', "+").replace('_', "/"))
}

fn scrypt_raw(
    password: &[u8],
    salt: &[u8],
    n: u64,
    r: u32,
    p: u32,
    dk_len: usize,
) -> Option<Vec<u8>> {
    if n < 2 || !n.is_power_of_two() || r == 0 || p == 0 || dk_len == 0 {
        return None;
    }
    // noble's default maxmem: 1 GiB.
    if 128u128 * r as u128 * (n as u128 + p as u128) > 1u128 << 30 {
        return None;
    }
    let params = scrypt::Params::new(n.trailing_zeros() as u8, r, p, 32).ok()?;
    let mut out = vec![0u8; dk_len];
    scrypt::scrypt(password, salt, &params, &mut out).ok()?;
    Some(out)
}

/// `scrypt$N$r$p$saltB64$hashB64`, exactly as TS.
pub fn hash_password(password: &str, salt: &[u8]) -> String {
    let hash = scrypt_raw(
        password.as_bytes(),
        salt,
        SCRYPT_N,
        SCRYPT_R,
        SCRYPT_P,
        SCRYPT_DK_LEN,
    )
    .expect("fixed scrypt parameters are valid");
    format!(
        "scrypt${SCRYPT_N}${SCRYPT_R}${SCRYPT_P}${}${}",
        base64_encode(salt),
        base64_encode(&hash)
    )
}

/// Verified in place of a missing hash, so unknown accounts take as long as known ones.
pub fn dummy_password_hash() -> String {
    format!(
        "scrypt${SCRYPT_N}${SCRYPT_R}${SCRYPT_P}${}${}",
        base64_encode(&[0u8; 16]),
        base64_encode(&[0u8; SCRYPT_DK_LEN])
    )
}

/// `parseInt(s, 10)`: optional whitespace and sign, then the leading digits.
fn js_parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let v: i64 = digits.parse().ok()?;
    Some(if neg { -v } else { v })
}

pub fn verify_password(password: &str, encoded: &str) -> bool {
    let parts: Vec<&str> = encoded.split('$').collect();
    if parts.len() != 6 || parts[0] != "scrypt" {
        return false;
    }
    let (Some(n), Some(r), Some(p)) = (
        js_parse_int(parts[1]),
        js_parse_int(parts[2]),
        js_parse_int(parts[3]),
    ) else {
        return false;
    };
    let (Ok(salt), Ok(expected)) = (
        crypto::base64_to_bytes(parts[4]),
        crypto::base64_to_bytes(parts[5]),
    ) else {
        return false;
    };
    if n <= 0 || r <= 0 || p <= 0 || r > u32::MAX as i64 || p > u32::MAX as i64 {
        return false;
    }
    match scrypt_raw(
        password.as_bytes(),
        &salt,
        n as u64,
        r as u32,
        p as u32,
        expected.len(),
    ) {
        Some(actual) => crypto::timing_safe_equal(&expected, &actual),
        None => false,
    }
}

pub fn pkce_challenge(verifier: &str) -> String {
    base64url_encode(&crypto::sha256(verifier.as_bytes()))
}

pub fn uuid_v7(now_ms: u64, rand: &[u8]) -> String {
    let mut b = [0u8; 16];
    for (i, byte) in b.iter_mut().take(6).enumerate() {
        *byte = ((now_ms >> (40 - 8 * i)) & 0xff) as u8;
    }
    b[6..16].copy_from_slice(&rand[..10]);
    b[6] = (b[6] & 0x0f) | 0x70;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = crypto::bytes_to_hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// HMAC-SHA256 keyed by the signing key over a database-wide counter and the timestamp. The
/// module RNG is seeded from the timestamp, so it can't produce secrets.
pub fn derive_secret(private_key: &[u8], counter: u64, micros: i64) -> [u8; 32] {
    crypto::hmac_sha256(
        private_key,
        format!("spacetimedb-auth secret {counter} {micros}").as_bytes(),
    )
}

// ---------------------------------------------------------------------------
// ES256 keys and PEM
// ---------------------------------------------------------------------------

/// An ES256 key pair with the encodings the module stores.
#[derive(Clone, Debug)]
pub struct Es256Keypair {
    pub private_key: [u8; 32],
    /// 65-byte uncompressed point.
    pub public_key: Vec<u8>,
    pub private_key_pem: String,
    pub public_key_pem: String,
    pub kid: String,
}

const SPKI_ALG_DER: [u8; 21] = [
    0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48,
    0xce, 0x3d, 0x03, 0x01, 0x07,
];

fn der_len_prefix(tag: u8, len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![tag, len as u8]
    } else if len < 0x100 {
        vec![tag, 0x81, len as u8]
    } else {
        vec![tag, 0x82, (len >> 8) as u8, len as u8]
    }
}

fn der_seq(body: &[u8]) -> Vec<u8> {
    [der_len_prefix(0x30, body.len()), body.to_vec()].concat()
}

fn pem_wrap(label: &str, der: &[u8]) -> String {
    let b64 = base64_encode(der);
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

fn pem_unwrap(label: &str, pem: &str) -> Result<Vec<u8>, String> {
    let prefix = format!("-----BEGIN {label}-----");
    let suffix = format!("-----END {label}-----");
    let (Some(start), Some(end)) = (pem.find(&prefix), pem.find(&suffix)) else {
        return Err(format!("PEM {label} block not found"));
    };
    if end < start + prefix.len() {
        return Err(format!("PEM {label} block not found"));
    }
    let inner: String = pem[start + prefix.len()..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    crypto::base64_to_bytes(&inner)
}

fn bit_string(public65: &[u8]) -> Vec<u8> {
    [
        vec![0x03, public65.len() as u8 + 1, 0x00],
        public65.to_vec(),
    ]
    .concat()
}

fn encode_public_key_pem(public65: &[u8]) -> String {
    pem_wrap(
        "PUBLIC KEY",
        &der_seq(&[SPKI_ALG_DER.to_vec(), bit_string(public65)].concat()),
    )
}

fn encode_private_key_pem(private: &[u8], public65: &[u8]) -> String {
    let named_curve: [u8; 10] = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
    let pub_bits = bit_string(public65);
    let ec_body = [
        vec![0x02, 0x01, 0x01],
        vec![0x04, private.len() as u8],
        private.to_vec(),
        vec![0xa0, named_curve.len() as u8],
        named_curve.to_vec(),
        vec![0xa1, pub_bits.len() as u8],
        pub_bits,
    ]
    .concat();
    let ec_der = der_seq(&ec_body);
    let p8 = [
        vec![0x02, 0x01, 0x00],
        SPKI_ALG_DER.to_vec(),
        der_len_prefix(0x04, ec_der.len()),
        ec_der,
    ]
    .concat();
    pem_wrap("PRIVATE KEY", &der_seq(&p8))
}

/// RFC 7638 thumbprint of the public JWK.
fn jwk_thumbprint(public65: &[u8]) -> String {
    let x = base64url_encode(&public65[1..33]);
    let y = base64url_encode(&public65[33..65]);
    let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
    base64url_encode(&crypto::sha256(canonical.as_bytes()))
}

pub fn from_private_key_bytes(private_key: &[u8]) -> Result<Es256Keypair, String> {
    if private_key.len() != 32 {
        return Err("ES256 private key must be 32 bytes".into());
    }
    let sk = SigningKey::from_slice(private_key).map_err(|_| "invalid private key".to_string())?;
    let public_key = sk
        .verifying_key()
        .to_encoded_point(false)
        .as_bytes()
        .to_vec();
    let mut private = [0u8; 32];
    private.copy_from_slice(private_key);
    Ok(Es256Keypair {
        private_key: private,
        private_key_pem: encode_private_key_pem(private_key, &public_key),
        public_key_pem: encode_public_key_pem(&public_key),
        kid: jwk_thumbprint(&public_key),
        public_key,
    })
}

/// The raw 32-byte key from a PKCS#8 PEM (the first `04 20` octet string, as TS).
pub fn private_key_from_pem(pem: &str) -> Result<Vec<u8>, String> {
    let der = pem_unwrap("PRIVATE KEY", pem)?;
    if der.len() > 33 {
        for i in 0..der.len() - 33 {
            if der[i] == 0x04 && der[i + 1] == 0x20 {
                return Ok(der[i + 2..i + 34].to_vec());
            }
        }
    }
    Err("could not extract raw private key from PKCS#8 PEM".into())
}

/// The 65-byte point from an SPKI PEM.
pub fn public_key_from_pem(pem: &str) -> Result<Vec<u8>, String> {
    let der = pem_unwrap("PUBLIC KEY", pem)?;
    if der.len() > 67 {
        for i in 0..der.len() - 67 {
            if der[i] == 0x03 && der[i + 1] == 0x42 && der[i + 2] == 0x00 && der[i + 3] == 0x04 {
                return Ok(der[i + 3..i + 68].to_vec());
            }
        }
    }
    Err("could not extract raw public key from SPKI PEM".into())
}

// ---------------------------------------------------------------------------
// JWT (ES256, RFC 6979 nonces, low-S like noble)
// ---------------------------------------------------------------------------

/// Sign `{header}.{payload_json}`; `payload_json` is the exact claims text.
pub fn sign_jwt(
    private_key: &[u8],
    payload_json: &str,
    kid: Option<&str>,
) -> Result<String, String> {
    let header = match kid {
        Some(k) => format!(
            r#"{{"alg":"ES256","typ":"JWT","kid":{}}}"#,
            js_json_string(k)
        ),
        None => r#"{"alg":"ES256","typ":"JWT"}"#.to_string(),
    };
    let input = format!(
        "{}.{}",
        base64url_encode(header.as_bytes()),
        base64url_encode(payload_json.as_bytes())
    );
    let sk = SigningKey::from_slice(private_key).map_err(|_| "invalid private key".to_string())?;
    let sig: Signature = sk.sign(input.as_bytes());
    let sig = sig.normalize_s().unwrap_or(sig);
    Ok(format!("{input}.{}", base64url_encode(&sig.to_bytes())))
}

/// The claims of an auth session token, in the TS key order.
pub fn session_claims_json(iss: &str, sub: &str, iat: i64, exp: i64, jti: &str) -> String {
    format!(
        r#"{{"iss":{},"sub":{},"aud":{},"iat":{iat},"exp":{exp},"jti":{}}}"#,
        js_json_string(iss),
        js_json_string(sub),
        js_json_string(iss),
        js_json_string(jti)
    )
}

#[derive(Clone, Debug, Default)]
pub struct VerifyJwtOptions<'a> {
    pub issuer: Option<&'a str>,
    pub audience: Option<&'a str>,
    /// Default 60.
    pub clock_tolerance_seconds: Option<i64>,
    pub now_seconds: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum JwtVerify {
    Ok {
        claims: Map<String, Value>,
        header: Map<String, Value>,
    },
    /// `malformed`, `bad-signature`, `expired`, `not-yet-valid`, `bad-issuer`, `bad-audience`.
    Err(&'static str),
}

pub fn verify_jwt(public_key: &[u8], token: &str, opts: &VerifyJwtOptions) -> JwtVerify {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return JwtVerify::Err("malformed");
    }
    let parse = |p: &str| -> Option<Value> {
        let bytes = base64url_decode(p).ok()?;
        serde_json::from_str(&String::from_utf8_lossy(&bytes)).ok()
    };
    let (Some(header), Some(claims)) = (parse(parts[0]), parse(parts[1])) else {
        return JwtVerify::Err("malformed");
    };
    let header = header.as_object().cloned().unwrap_or_default();
    if header.get("alg").and_then(Value::as_str) != Some("ES256") {
        return JwtVerify::Err("bad-signature");
    }
    let Ok(sig) = base64url_decode(parts[2]) else {
        return JwtVerify::Err("malformed");
    };
    if sig.len() != 64 {
        return JwtVerify::Err("bad-signature");
    }
    let ok = (|| {
        let vk = VerifyingKey::from_sec1_bytes(public_key).ok()?;
        let sig = Signature::from_slice(&sig).ok()?;
        // noble rejects high-S signatures by default.
        if sig.normalize_s().is_some() {
            return None;
        }
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .ok()
    })();
    if ok.is_none() {
        return JwtVerify::Err("bad-signature");
    }
    let claims = claims.as_object().cloned().unwrap_or_default();
    let now = opts.now_seconds as f64;
    let skew = opts.clock_tolerance_seconds.unwrap_or(60) as f64;
    if let Some(exp) = claims.get("exp").and_then(Value::as_f64)
        && exp + skew < now
    {
        return JwtVerify::Err("expired");
    }
    if let Some(nbf) = claims.get("nbf").and_then(Value::as_f64)
        && nbf - skew > now
    {
        return JwtVerify::Err("not-yet-valid");
    }
    if let Some(issuer) = opts.issuer
        && claims.get("iss").and_then(Value::as_str) != Some(issuer)
    {
        return JwtVerify::Err("bad-issuer");
    }
    if let Some(audience) = opts.audience {
        let matches = match claims.get("aud") {
            Some(Value::Array(a)) => a.iter().any(|v| v.as_str() == Some(audience)),
            Some(Value::String(s)) => s == audience,
            _ => false,
        };
        if !matches {
            return JwtVerify::Err("bad-audience");
        }
    }
    JwtVerify::Ok { claims, header }
}

// ---------------------------------------------------------------------------
// Request trust
// ---------------------------------------------------------------------------

/// A header set by a trusted proxy after it removes any client-supplied value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustedProxyHeader {
    CfConnectingIp,
    XRealIp,
    XForwardedFor,
}

impl TrustedProxyHeader {
    pub fn header_name(self) -> &'static str {
        match self {
            Self::CfConnectingIp => "cf-connecting-ip",
            Self::XRealIp => "x-real-ip",
            Self::XForwardedFor => "x-forwarded-for",
        }
    }
}

fn has_control_character(s: &str) -> bool {
    s.encode_utf16().any(|c| c <= 0x1f || c == 0x7f)
}

/// `headers.get(name)`: every value for the name, joined with `, `.
pub fn header(req: &Request, name: &str) -> Option<String> {
    let values: Vec<String> = req
        .headers()
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect();
    if values.is_empty() {
        None
    } else {
        Some(values.join(", "))
    }
}

pub fn client_key(req: &Request, trusted: Option<TrustedProxyHeader>) -> Option<String> {
    let value = header(req, trusted?.header_name())?;
    if value.is_empty() {
        return None;
    }
    let first = js_trim(value.split(',').next().unwrap_or(""));
    if first.is_empty() || utf16_len(&first) > 128 {
        None
    } else {
        Some(first)
    }
}

pub fn user_agent(req: &Request) -> Option<String> {
    let value = js_trim(&header(req, "user-agent")?);
    if !value.is_empty() && utf16_len(&value) <= 512 {
        Some(value)
    } else {
        None
    }
}

/// JS `String.prototype.trim` (JS whitespace and line terminators).
fn js_trim(s: &str) -> String {
    let is_ws = |c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    };
    s.trim_matches(is_ws).to_string()
}

/// JS `decodeURIComponent`: errors on malformed escapes and invalid UTF-8.
pub fn decode_uri_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// An application-relative redirect path, or `None` if unsafe.
pub fn safe_redirect_path(value: &str) -> Option<String> {
    let len = utf16_len(value);
    if len == 0 || len > 2048 || !value.starts_with('/') || value.starts_with("//") {
        return None;
    }
    if value.contains('\\') || value.contains('#') || has_control_character(value) {
        return None;
    }
    let decoded = decode_uri_component(value)?;
    if !decoded.starts_with('/')
        || decoded.starts_with("//")
        || decoded.contains('\\')
        || decoded.contains('#')
        || has_control_character(&decoded)
    {
        return None;
    }
    Some(value.to_string())
}

// ---------------------------------------------------------------------------
// Install, admin, config
// ---------------------------------------------------------------------------

/// Call from your `init` reducer: installs rate-limit, makes the publisher the first admin and
/// schedules the expiry sweep every 60 s.
pub fn install(ctx: &ReducerContext) {
    rate_limit::install(ctx);
    if ctx
        .db
        .auth_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        ctx.db.auth_admin_identity().insert(AuthAdminIdentity {
            identity: ctx.sender(),
            added_at: ctx.timestamp,
        });
    }
    if ctx.db.auth_sweeper_tick().count() == 0 {
        ctx.db.auth_sweeper_tick().insert(AuthSweeperTick {
            scheduled_id: 0,
            scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(60 * ONE_SECOND_MICROS)),
        });
    }
}

fn require_admin(ctx: &ReducerContext) -> Result<(), String> {
    if ctx
        .db
        .auth_admin_identity()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    Ok(())
}

/// Admin only. The first call needs `es256_private_key_pem` (PKCS#8, generated outside the module).
#[allow(clippy::too_many_arguments)]
#[spacetimedb::reducer]
pub fn set_auth_config(
    ctx: &ReducerContext,
    issuer_url: String,
    base_url: Option<String>,
    cookie_name: Option<String>,
    session_ttl_seconds: Option<u64>,
    es256_private_key_pem: Option<String>,
    google_client_id: Option<String>,
    google_client_secret: Option<String>,
    github_client_id: Option<String>,
    github_client_secret: Option<String>,
) -> Result<(), String> {
    require_admin(ctx)?;
    let existing = ctx.db.auth_config().singleton().find(true);
    let mut key = existing.as_ref().map(|e| {
        (
            e.es256_private_key_pem.clone(),
            e.es256_public_key_pem.clone(),
            e.key_id.clone(),
        )
    });
    if let Some(pem) = es256_private_key_pem.as_deref().filter(|p| !p.is_empty()) {
        let kp = private_key_from_pem(pem)
            .and_then(|raw| from_private_key_bytes(&raw))
            .map_err(|e| format!("{}:{e}", errors::INVALID_PRIVATE_KEY_PEM))?;
        key = Some((kp.private_key_pem, kp.public_key_pem, kp.kid));
    }
    let Some((private_pem, public_pem, kid)) = key else {
        return Err(errors::SIGNING_KEY_REQUIRED.into());
    };
    let row = AuthConfig {
        singleton: true,
        base_url: base_url
            .or_else(|| existing.as_ref().map(|e| e.base_url.clone()))
            .unwrap_or_else(|| issuer_url.clone()),
        issuer_url,
        cookie_name: cookie_name
            .or_else(|| existing.as_ref().map(|e| e.cookie_name.clone()))
            .unwrap_or_else(|| DEFAULT_COOKIE_NAME.into()),
        session_ttl_seconds: session_ttl_seconds
            .or_else(|| existing.as_ref().map(|e| e.session_ttl_seconds))
            .unwrap_or(DEFAULT_SESSION_TTL_SECONDS),
        es256_private_key_pem: private_pem,
        es256_public_key_pem: public_pem,
        key_id: kid,
        token_counter: existing.as_ref().map_or(0, |e| e.token_counter),
        google_client_id: google_client_id
            .or_else(|| existing.as_ref().and_then(|e| e.google_client_id.clone())),
        google_client_secret: google_client_secret.or_else(|| {
            existing
                .as_ref()
                .and_then(|e| e.google_client_secret.clone())
        }),
        github_client_id: github_client_id
            .or_else(|| existing.as_ref().and_then(|e| e.github_client_id.clone())),
        github_client_secret: github_client_secret.or_else(|| {
            existing
                .as_ref()
                .and_then(|e| e.github_client_secret.clone())
        }),
        updated_at: ctx.timestamp,
    };
    if existing.is_some() {
        ctx.db.auth_config().singleton().update(row);
    } else {
        ctx.db.auth_config().insert(row);
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn add_auth_admin(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .auth_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        ctx.db.auth_admin_identity().insert(AuthAdminIdentity {
            identity,
            added_at: ctx.timestamp,
        });
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn remove_auth_admin(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    require_admin(ctx)?;
    if ctx
        .db
        .auth_admin_identity()
        .identity()
        .find(identity)
        .is_none()
    {
        return Ok(());
    }
    if ctx.db.auth_admin_identity().count() <= 1 {
        return Err(errors::LAST_ADMIN.into());
    }
    ctx.db.auth_admin_identity().identity().delete(identity);
    Ok(())
}

#[spacetimedb::procedure]
pub fn get_auth_public_key(ctx: &mut ProcedureContext) -> Result<AuthPubKey, String> {
    ctx.with_tx(|tx| {
        let cfg = tx
            .db
            .auth_config()
            .singleton()
            .find(true)
            .ok_or(errors::CONFIG_MISSING.to_string())?;
        Ok(AuthPubKey {
            public_key_pem: cfg.es256_public_key_pem,
            key_id: cfg.key_id,
            issuer_url: cfg.issuer_url,
        })
    })
}

// ---------------------------------------------------------------------------
// Secrets, sessions and the caller
// ---------------------------------------------------------------------------

/// 32 bytes that are unpredictable without the signing key. Advances the config counter.
pub fn secret_bytes(tx: &ReducerContext) -> Result<[u8; 32], String> {
    let cfg = tx
        .db
        .auth_config()
        .singleton()
        .find(true)
        .ok_or(errors::CONFIG_MISSING.to_string())?;
    let token_counter = cfg.token_counter + 1;
    let key = private_key_from_pem(&cfg.es256_private_key_pem)?;
    tx.db.auth_config().singleton().update(AuthConfig {
        token_counter,
        ..cfg
    });
    Ok(derive_secret(&key, token_counter, now_micros(tx.timestamp)))
}

pub fn new_token(tx: &ReducerContext) -> Result<String, String> {
    Ok(base64url_encode(&secret_bytes(tx)?))
}

pub fn new_id(tx: &ReducerContext) -> Result<String, String> {
    let ms = (now_micros(tx.timestamp) / 1000).max(0) as u64;
    Ok(uuid_v7(ms, &secret_bytes(tx)?))
}

/// Deletes a session and every connection bound to it.
pub fn delete_session(tx: &ReducerContext, session: &AuthSession) {
    let bindings: Vec<_> = tx
        .db
        .auth_connection_binding()
        .session_id()
        .filter(session.session_id.as_str())
        .collect();
    for b in bindings {
        tx.db
            .auth_connection_binding()
            .stdb_identity()
            .delete(b.stdb_identity);
    }
    tx.db
        .auth_session()
        .session_id()
        .delete(&session.session_id);
}

pub fn is_live(session: &AuthSession, now: Timestamp) -> bool {
    session.expires_at > now
}

/// The live session for a signed session token.
pub fn token_session(tx: &ReducerContext, cfg: &AuthConfig, token: &str) -> Option<AuthSession> {
    let public = public_key_from_pem(&cfg.es256_public_key_pem).ok()?;
    let opts = VerifyJwtOptions {
        issuer: Some(&cfg.issuer_url),
        now_seconds: now_micros(tx.timestamp).div_euclid(ONE_SECOND_MICROS),
        ..Default::default()
    };
    let JwtVerify::Ok { claims, .. } = verify_jwt(&public, token, &opts) else {
        return None;
    };
    let jti = claims
        .get("jti")
        .and_then(Value::as_str)
        .filter(|j| !j.is_empty())?;
    let session = tx.db.auth_session().session_id().find(jti.to_string())?;
    if claims.get("sub").and_then(Value::as_str) != Some(session.user_id.as_str()) {
        return None;
    }
    is_live(&session, tx.timestamp).then_some(session)
}

/// The live session presented by a request's bearer token or session cookie.
pub fn request_session(
    tx: &ReducerContext,
    cfg: &AuthConfig,
    req: &Request,
) -> Option<AuthSession> {
    let token = read_bearer(req, &cfg.cookie_name)?;
    token_session(tx, cfg, &token)
}

/// The user id of the live session presented to an HTTP handler (which has no caller identity).
pub fn request_user_id(tx: &ReducerContext, req: &Request) -> Option<String> {
    let cfg = tx.db.auth_config().singleton().find(true)?;
    request_session(tx, &cfg, req).map(|s| s.user_id)
}

fn bound_user(
    binding: Option<AuthConnectionBinding>,
    session: impl Fn(&str) -> Option<AuthSession>,
    user: impl Fn(&str) -> Option<AuthUser>,
    now: Option<Timestamp>,
) -> Option<AuthUser> {
    let binding = binding?;
    let s = session(&binding.session_id)?;
    if let Some(now) = now
        && s.expires_at <= now
    {
        return None;
    }
    user(&binding.user_id)
}

/// The caller's `auth_user` row if its connection is bound to a live session.
pub fn find_caller_user(ctx: &ReducerContext) -> Option<AuthUser> {
    bound_user(
        ctx.db
            .auth_connection_binding()
            .stdb_identity()
            .find(ctx.sender()),
        |id| ctx.db.auth_session().session_id().find(id.to_string()),
        |id| ctx.db.auth_user().user_id().find(id.to_string()),
        Some(ctx.timestamp),
    )
}

/// From a view: views have no clock, so an expired session stops counting within one sweep.
pub fn find_caller_user_in_view(ctx: &ViewContext) -> Option<AuthUser> {
    bound_user(
        ctx.db
            .auth_connection_binding()
            .stdb_identity()
            .find(ctx.sender()),
        |id| ctx.db.auth_session().session_id().find(id.to_string()),
        |id| ctx.db.auth_user().user_id().find(id.to_string()),
        None,
    )
}

pub fn get_caller_user_id(ctx: &ReducerContext) -> Option<String> {
    find_caller_user(ctx).map(|u| u.user_id)
}

/// The caller's user id, or `Err(auth.not_authenticated)`.
pub fn require_caller_user_id(ctx: &ReducerContext) -> Result<String, String> {
    get_caller_user_id(ctx).ok_or_else(|| errors::NOT_AUTHENTICATED.into())
}

// ---------------------------------------------------------------------------
// Client operations
// ---------------------------------------------------------------------------

/// Call once after each connect: binds this connection to the session behind `session_token`.
#[spacetimedb::reducer]
pub fn auth_link_connection(ctx: &ReducerContext, session_token: String) -> Result<(), String> {
    let cfg = ctx
        .db
        .auth_config()
        .singleton()
        .find(true)
        .ok_or(errors::CONFIG_MISSING.to_string())?;
    let session =
        token_session(ctx, &cfg, &session_token).ok_or(errors::INVALID_TOKEN.to_string())?;
    let binding = AuthConnectionBinding {
        stdb_identity: ctx.sender(),
        user_id: session.user_id,
        session_id: session.session_id,
        linked_at: ctx.timestamp,
    };
    if ctx
        .db
        .auth_connection_binding()
        .stdb_identity()
        .find(ctx.sender())
        .is_some()
    {
        ctx.db
            .auth_connection_binding()
            .stdb_identity()
            .update(binding);
    } else {
        ctx.db.auth_connection_binding().insert(binding);
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn auth_unlink_connection(ctx: &ReducerContext) {
    ctx.db
        .auth_connection_binding()
        .stdb_identity()
        .delete(ctx.sender());
}

/// Set the caller's display name / image; an empty string clears it, `None` keeps it.
#[spacetimedb::reducer]
pub fn auth_update_profile(
    ctx: &ReducerContext,
    name: Option<String>,
    image: Option<String>,
) -> Result<(), String> {
    let user_id = require_caller_user_id(ctx)?;
    let user = ctx
        .db
        .auth_user()
        .user_id()
        .find(&user_id)
        .ok_or(errors::USER_NOT_FOUND.to_string())?;
    let next = |value: Option<String>, current: Option<String>| match value {
        None => current,
        Some(v) => Some(js_trim(&v)).filter(|t| !t.is_empty()),
    };
    let name = next(name, user.name.clone());
    let image = next(image, user.image.clone());
    if name
        .as_deref()
        .is_some_and(|n| utf16_len(n) > MAX_PROFILE_NAME_LEN)
    {
        return Err(format!(
            "{}:max={MAX_PROFILE_NAME_LEN}",
            errors::NAME_TOO_LONG
        ));
    }
    if image
        .as_deref()
        .is_some_and(|n| utf16_len(n) > MAX_PROFILE_IMAGE_LEN)
    {
        return Err(format!(
            "{}:max={MAX_PROFILE_IMAGE_LEN}",
            errors::IMAGE_TOO_LONG
        ));
    }
    ctx.db.auth_user().user_id().update(AuthUser {
        name,
        image,
        updated_at: ctx.timestamp,
        ..user
    });
    Ok(())
}

/// Admin action for revoking any user's session.
#[spacetimedb::reducer]
pub fn auth_revoke_session(ctx: &ReducerContext, session_id: String) -> Result<(), String> {
    require_admin(ctx)?;
    if let Some(s) = ctx.db.auth_session().session_id().find(&session_id) {
        delete_session(ctx, &s);
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn auth_revoke_my_session(ctx: &ReducerContext, session_id: String) -> Result<(), String> {
    let user_id = require_caller_user_id(ctx)?;
    let Some(s) = ctx.db.auth_session().session_id().find(&session_id) else {
        return Ok(());
    };
    if s.user_id != user_id {
        return Err(errors::SESSION_NOT_OWNED.into());
    }
    delete_session(ctx, &s);
    Ok(())
}

#[spacetimedb::procedure]
pub fn auth_list_my_sessions(ctx: &mut ProcedureContext) -> Result<MySessions, String> {
    let sender = ctx.sender();
    Ok(ctx.with_tx(|tx| {
        let Some(binding) = tx.db.auth_connection_binding().stdb_identity().find(sender) else {
            return MySessions { sessions: vec![] };
        };
        match tx.db.auth_session().session_id().find(&binding.session_id) {
            Some(current) if is_live(&current, tx.timestamp) => {}
            _ => return MySessions { sessions: vec![] },
        }
        let mut sessions: Vec<MySession> = tx
            .db
            .auth_session()
            .user_id()
            .filter(&binding.user_id)
            .filter(|s| is_live(s, tx.timestamp))
            .map(|s| MySession {
                is_current: s.session_id == binding.session_id,
                session_id: s.session_id,
                expires_at: s.expires_at,
                created_at: s.created_at,
                ip_address: s.ip_address,
                user_agent: s.user_agent,
            })
            .collect();
        // Newest first; stable for ties, like Array.prototype.sort.
        sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        MySessions { sessions }
    }))
}

#[spacetimedb::procedure]
pub fn auth_whoami(ctx: &mut ProcedureContext) -> Result<WhoAmI, String> {
    let sender = ctx.sender();
    let user_id = ctx.with_tx(|tx| get_caller_user_id(tx));
    Ok(WhoAmI {
        user_id,
        sender_identity_hex: sender.to_hex().to_string(),
    })
}

#[spacetimedb::view(accessor = my_auth_user, public)]
fn my_auth_user(ctx: &ViewContext) -> Vec<AuthUser> {
    find_caller_user_in_view(ctx).into_iter().collect()
}

/// Deletes every expired session, verification and OAuth state. Scheduled every 60 s.
#[spacetimedb::reducer]
pub fn auth_sweep(ctx: &ReducerContext, _tick: AuthSweeperTick) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err(errors::NOT_AUTHORIZED.into());
    }
    let now = ctx.timestamp;
    let sessions: Vec<_> = ctx.db.auth_session().expires_at().filter(..=now).collect();
    for s in sessions {
        delete_session(ctx, &s);
    }
    let verifications: Vec<_> = ctx
        .db
        .auth_verification()
        .expires_at()
        .filter(..=now)
        .collect();
    for v in verifications {
        ctx.db
            .auth_verification()
            .verification_id()
            .delete(&v.verification_id);
    }
    let states: Vec<_> = ctx
        .db
        .auth_oauth_state()
        .expires_at()
        .filter(..=now)
        .collect();
    for s in states {
        ctx.db.auth_oauth_state().state().delete(&s.state);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

/// Delivers verification and reset mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailParams {
    pub to: String,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
}

pub type SendMailFn = fn(&mut HandlerContext, &MailParams);

/// The TS `client(options)` settings.
#[derive(Clone, Copy)]
pub struct AuthHttpOptions {
    /// Enables IP limits and stored session IPs from a trusted proxy header.
    pub trusted_proxy_header: Option<TrustedProxyHeader>,
    /// Default true. Set false only for local plain-HTTP development.
    pub secure_cookies: bool,
    /// Verification and reset routes return `mailer_not_configured` without it.
    pub send_mail: Option<SendMailFn>,
    pub app_name: Option<&'static str>,
    /// Where the email verification link lands. Default `/`.
    pub email_verified_redirect: Option<&'static str>,
}

impl AuthHttpOptions {
    pub const DEFAULT: Self = Self {
        trusted_proxy_header: None,
        secure_cookies: true,
        send_mail: None,
        app_name: None,
        email_verified_redirect: None,
    };
}

impl Default for AuthHttpOptions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// An HTTP error: `{ "error": code }` with `status`.
#[derive(Clone, Debug)]
pub struct HttpError {
    pub code: String,
    pub status: u16,
}

fn herr(code: impl Into<String>, status: u16) -> HttpError {
    HttpError {
        code: code.into(),
        status,
    }
}

type HResult<T> = Result<T, HttpError>;

pub fn make_cookie(name: &str, value: &str, max_age_seconds: Option<i64>, secure: bool) -> String {
    let mut parts = vec![format!("{name}={value}"), "Path=/".to_string()];
    if let Some(m) = max_age_seconds {
        parts.push(format!("Max-Age={m}"));
    }
    parts.push("HttpOnly".into());
    if secure {
        parts.push("Secure".into());
    }
    parts.push("SameSite=Lax".into());
    parts.join("; ")
}

pub fn clear_cookie(name: &str, secure: bool) -> String {
    make_cookie(name, "", Some(0), secure)
}

pub fn parse_cookies(header: Option<&str>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let Some(header) = header else { return out };
    for part in header.split(';') {
        let Some(eq) = part.find('=') else { continue };
        let k = js_trim(&part[..eq]);
        let v = js_trim(&part[eq + 1..]);
        if !k.is_empty() {
            match out.iter_mut().find(|(ek, _)| ek == &k) {
                Some(e) => e.1 = v,
                None => out.push((k, v)),
            }
        }
    }
    out
}

fn cookie(req: &Request, name: &str) -> Option<String> {
    parse_cookies(header(req, "cookie").as_deref())
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// Bearer token, else the session cookie.
pub fn read_bearer(req: &Request, cookie_name: &str) -> Option<String> {
    if let Some(auth) = header(req, "authorization")
        && auth.len() >= 7
        && auth.is_char_boundary(7)
        && auth[..7].eq_ignore_ascii_case("bearer ")
    {
        return Some(js_trim(&auth[7..]));
    }
    cookie(req, cookie_name)
}

/// Query parameters (no `+` handling; malformed escapes are skipped), as the TS helper.
pub fn parse_query_string(uri: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let Some(q) = uri.find('?') else { return out };
    for pair in uri[q + 1..].split('&') {
        let (k, v) = match pair.find('=') {
            None => (decode_uri_component(pair), Some(String::new())),
            Some(eq) => (
                decode_uri_component(&pair[..eq]),
                decode_uri_component(&pair[eq + 1..]),
            ),
        };
        if let (Some(k), Some(v)) = (k, v) {
            // Like assigning to a JS object: a repeated key keeps its first position.
            match out.iter_mut().find(|(ek, _)| ek == &k) {
                Some(e) => e.1 = v,
                None => out.push((k, v)),
            }
        }
    }
    out
}

fn query_param(req: &Request, name: &str) -> Option<String> {
    parse_query_string(&req.uri().to_string())
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

fn response(status: u16, headers: &[(&str, String)], body: Vec<u8>) -> Response {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(
            *k,
            HeaderValue::from_str(v).unwrap_or_else(|_| HeaderValue::from_static("")),
        );
    }
    b.body(Body::from_bytes(body)).expect("valid response")
}

pub fn json_response(body: &Value, status: u16, cookies: &[String]) -> Response {
    let mut headers = vec![("content-type", "application/json".to_string())];
    headers.extend(cookies.iter().map(|c| ("set-cookie", c.clone())));
    response(status, &headers, body.to_string().into_bytes())
}

pub fn error_response(code: &str, status: u16, extra: &[(&str, String)]) -> Response {
    let mut headers = vec![("content-type", "application/json".to_string())];
    headers.extend(extra.iter().cloned());
    response(
        status,
        &headers,
        format!(r#"{{"error":{}}}"#, js_json_string(code)).into_bytes(),
    )
}

pub fn redirect_response(location: &str, cookies: &[String]) -> Response {
    let mut headers = vec![("location", location.to_string())];
    headers.extend(cookies.iter().map(|c| ("set-cookie", c.clone())));
    response(302, &headers, Vec::new())
}

fn require_config(tx: &ReducerContext) -> HResult<AuthConfig> {
    tx.db
        .auth_config()
        .singleton()
        .find(true)
        .ok_or(herr("config_missing", 500))
}

fn internal(e: String) -> HttpError {
    log::warn!("auth: {e}");
    herr("internal_error", 500)
}

fn body_json(req: Request) -> Option<Map<String, Value>> {
    let text = req.into_body().into_string().ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .as_object()
        .cloned()
}

/// A string field, or `Err` if present but not a string. Missing/null → `None`.
fn str_field(body: &Map<String, Value>, key: &str) -> Result<Option<String>, ()> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(()),
    }
}

/// Run a handler body, turning `HttpError`s into JSON error responses (the TS `client()` boundary).
fn route(f: impl FnOnce() -> HResult<Response>) -> Response {
    f().unwrap_or_else(|e| error_response(&e.code, e.status, &[]))
}

fn with_tx<T>(ctx: &mut HandlerContext, body: impl Fn(&TxContext) -> HResult<T>) -> HResult<T> {
    ctx.try_with_tx(body)
}

// ---------------------------------------------------------------------------
// Rate limits
// ---------------------------------------------------------------------------

pub mod limits {
    use crate::rate_limit::{RateLimitPolicy, policy};
    pub const PASSWORD_SIGNUP: RateLimitPolicy = policy("auth.password.signup", 5, 3600);
    pub const PASSWORD_LOGIN_IP: RateLimitPolicy = policy("auth.password.login.ip", 30, 300);
    pub const PASSWORD_LOGIN_EMAIL: RateLimitPolicy = policy("auth.password.login.email", 10, 300);
    pub const PASSWORD_FORGOT_IP: RateLimitPolicy = policy("auth.password.forgot.ip", 5, 3600);
    pub const PASSWORD_FORGOT_EMAIL: RateLimitPolicy =
        policy("auth.password.forgot.email", 3, 3600);
    pub const PASSWORD_RESET: RateLimitPolicy = policy("auth.password.reset", 5, 900);
    pub const OAUTH_START: RateLimitPolicy = policy("auth.oauth.start", 30, 300);
    pub const EMAIL_VERIFY_REQUEST: RateLimitPolicy = policy("auth.email.verify_request", 5, 3600);
}

fn rate_limit_response(r: &RateLimitResult) -> Response {
    error_response(
        "rate_limited",
        429,
        &[
            ("retry-after", r.retry_after_seconds.to_string()),
            ("x-ratelimit-limit", r.limit.to_string()),
            ("x-ratelimit-remaining", r.remaining.to_string()),
            (
                "x-ratelimit-reset",
                now_micros(r.reset_at)
                    .div_euclid(ONE_SECOND_MICROS)
                    .to_string(),
            ),
        ],
    )
}

/// Spend one unit per check, each in its own transaction; the first block wins.
fn enforce_rate_limits(
    ctx: &mut HandlerContext,
    checks: &[(RateLimitPolicy, String)],
) -> HResult<Option<Response>> {
    for (policy, actor) in checks {
        let key = utf16_prefix(&js_trim(&actor.to_lowercase()), 256);
        let result = ctx
            .with_tx(|tx| policy.consume(tx, &key, 1))
            .map_err(internal)?;
        if !result.allowed {
            return Ok(Some(rate_limit_response(&result)));
        }
    }
    Ok(None)
}

fn enforce_ip_rate_limit(
    ctx: &mut HandlerContext,
    req: &Request,
    policy: RateLimitPolicy,
    trusted: Option<TrustedProxyHeader>,
) -> HResult<Option<Response>> {
    match client_key(req, trusted) {
        Some(key) => enforce_rate_limits(ctx, &[(policy, format!("ip:{key}"))]),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Sessions over HTTP
// ---------------------------------------------------------------------------

/// What `issue_session` returns: the session id, signed token, expiry (s) and cookie.
#[derive(Clone, Debug)]
pub struct IssuedSession {
    pub session_id: String,
    pub token: String,
    pub expires_at: i64,
    pub cookie: String,
}

/// Inserts a session and returns its signed token and cookie.
pub fn issue_session(
    tx: &ReducerContext,
    cfg: &AuthConfig,
    user_id: &str,
    ip_address: Option<String>,
    user_agent: Option<String>,
    opts: &AuthHttpOptions,
) -> Result<IssuedSession, String> {
    let session_id = new_token(tx)?;
    let now_sec = now_micros(tx.timestamp).div_euclid(ONE_SECOND_MICROS);
    let ttl = cfg.session_ttl_seconds as i64;
    tx.db.auth_session().insert(AuthSession {
        session_id: session_id.clone(),
        user_id: user_id.to_string(),
        expires_at: plus_seconds(tx.timestamp, ttl),
        ip_address,
        user_agent,
        created_at: tx.timestamp,
    });
    let claims = session_claims_json(
        &cfg.issuer_url,
        user_id,
        now_sec,
        now_sec + ttl,
        &session_id,
    );
    let token = sign_jwt(
        &private_key_from_pem(&cfg.es256_private_key_pem)?,
        &claims,
        Some(&cfg.key_id),
    )?;
    Ok(IssuedSession {
        cookie: make_cookie(&cfg.cookie_name, &token, Some(ttl), opts.secure_cookies),
        session_id,
        token,
        expires_at: now_sec + ttl,
    })
}

fn public_user(user: &AuthUser) -> Value {
    let mut m = Map::new();
    m.insert("userId".into(), Value::String(user.user_id.clone()));
    m.insert("email".into(), Value::String(user.email.clone()));
    m.insert("emailVerified".into(), Value::Bool(user.email_verified));
    if let Some(n) = &user.name {
        m.insert("name".into(), Value::String(n.clone()));
    }
    if let Some(i) = &user.image {
        m.insert("image".into(), Value::String(i.clone()));
    }
    Value::Object(m)
}

fn password_account(tx: &ReducerContext, user_id: &str) -> Option<AuthAccount> {
    tx.db
        .auth_account()
        .user_id()
        .filter(user_id)
        .find(|a| a.provider_id == "password")
}

fn new_salt(tx: &ReducerContext) -> Result<Vec<u8>, String> {
    Ok(secret_bytes(tx)?[..16].to_vec())
}

fn normalize_email(email: &str) -> String {
    js_trim(&email.to_lowercase())
}

/// `POST` `{ email, password, name? }` → `{ user: { userId, email }, token }` and a session cookie.
pub fn password_signup(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    let ip = client_key(&req, opts.trusted_proxy_header);
    let ua = user_agent(&req);
    route(|| {
        let body = body_json(req).ok_or(herr("invalid_request", 400))?;
        let bad = || herr("invalid_request", 400);
        let email = str_field(&body, "email")
            .map_err(|_| bad())?
            .filter(|s| !s.is_empty())
            .ok_or_else(bad)?;
        let password = str_field(&body, "password")
            .map_err(|_| bad())?
            .filter(|s| !s.is_empty())
            .ok_or_else(bad)?;
        let name = str_field(&body, "name").map_err(|_| bad())?;
        if utf16_len(&password) < MIN_PASSWORD_LEN {
            return Err(herr("password_too_short", 400));
        }
        if utf16_len(&password) > MAX_PASSWORD_LEN {
            return Err(herr("password_too_long", 400));
        }
        let email = normalize_email(&email);
        if email.is_empty() || utf16_len(&email) > MAX_EMAIL_LEN {
            return Err(herr("invalid_email", 400));
        }
        if name
            .as_deref()
            .is_some_and(|n| utf16_len(n) > MAX_SIGNUP_NAME_LEN)
        {
            return Err(herr("name_too_long", 400));
        }
        let mut checks = vec![(limits::PASSWORD_SIGNUP, format!("email:{email}"))];
        if let Some(ip) = &ip {
            checks.push((limits::PASSWORD_SIGNUP, format!("ip:{ip}")));
        }
        if let Some(r) = enforce_rate_limits(ctx, &checks)? {
            return Ok(r);
        }
        let email_free = |tx: &ReducerContext| -> HResult<()> {
            if tx.db.auth_user().email().find(&email).is_some() {
                return Err(herr("email_taken", 409));
            }
            Ok(())
        };
        let salt = with_tx(ctx, |tx| {
            email_free(tx)?;
            new_salt(tx).map_err(|_| herr("config_missing", 500))
        })?;
        let password_hash = hash_password(&password, &salt);
        let out = with_tx(ctx, |tx| {
            let cfg = require_config(tx)?;
            email_free(tx)?;
            let user_id = new_id(tx).map_err(internal)?;
            tx.db.auth_user().insert(AuthUser {
                user_id: user_id.clone(),
                email: email.clone(),
                email_verified: false,
                name: name.clone(),
                image: None,
                created_at: tx.timestamp,
                updated_at: tx.timestamp,
            });
            tx.db.auth_account().insert(AuthAccount {
                account_id: new_id(tx).map_err(internal)?,
                user_id: user_id.clone(),
                provider_id: "password".into(),
                provider_account_id: email.clone(),
                password_hash: Some(password_hash.clone()),
                access_token: None,
                refresh_token: None,
                access_token_expires_at: None,
                created_at: tx.timestamp,
                updated_at: tx.timestamp,
            });
            let s = issue_session(tx, &cfg, &user_id, ip.clone(), ua.clone(), opts)
                .map_err(internal)?;
            Ok((user_id, s))
        })?;
        Ok(json_response(
            &serde_json::json!({ "user": { "userId": out.0, "email": email }, "token": out.1.token }),
            200,
            &[out.1.cookie],
        ))
    })
}

/// `POST` `{ email, password }` → `{ userId, token }` and a session cookie.
pub fn password_login(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    let ip = client_key(&req, opts.trusted_proxy_header);
    let ua = user_agent(&req);
    route(|| {
        let body = body_json(req).ok_or(herr("invalid_request", 400))?;
        let bad = || herr("invalid_request", 400);
        let email = str_field(&body, "email")
            .map_err(|_| bad())?
            .filter(|s| !s.is_empty())
            .ok_or_else(bad)?;
        let password = str_field(&body, "password")
            .map_err(|_| bad())?
            .filter(|s| !s.is_empty())
            .ok_or_else(bad)?;
        if utf16_len(&password) > MAX_PASSWORD_LEN {
            return Err(herr("invalid_credentials", 401));
        }
        let email = normalize_email(&email);
        if email.is_empty() || utf16_len(&email) > MAX_EMAIL_LEN {
            return Err(herr("invalid_credentials", 401));
        }
        let mut checks = Vec::new();
        if let Some(ip) = &ip {
            checks.push((limits::PASSWORD_LOGIN_IP, format!("ip:{ip}")));
        }
        checks.push((limits::PASSWORD_LOGIN_EMAIL, format!("email:{email}")));
        if let Some(r) = enforce_rate_limits(ctx, &checks)? {
            return Ok(r);
        }
        let stored = with_tx(ctx, |tx| {
            require_config(tx)?;
            Ok(tx
                .db
                .auth_user()
                .email()
                .find(&email)
                .and_then(|u| password_account(tx, &u.user_id)))
        })?;
        // Hash outside the transaction, and hash a dummy for unknown accounts so the response
        // time doesn't reveal which emails are registered.
        let encoded = stored
            .as_ref()
            .and_then(|a| a.password_hash.clone())
            .unwrap_or_else(dummy_password_hash);
        let valid = verify_password(&password, &encoded);
        let Some(stored) = stored.filter(|a| a.password_hash.is_some() && valid) else {
            return Err(herr("invalid_credentials", 401));
        };
        let session = with_tx(ctx, |tx| {
            let cfg = require_config(tx)?;
            // Reject a password that changed while it was being verified.
            if password_account(tx, &stored.user_id).and_then(|a| a.password_hash)
                != stored.password_hash
            {
                return Err(herr("invalid_credentials", 401));
            }
            issue_session(tx, &cfg, &stored.user_id, ip.clone(), ua.clone(), opts).map_err(internal)
        })?;
        Ok(json_response(
            &serde_json::json!({ "userId": stored.user_id, "token": session.token }),
            200,
            &[session.cookie],
        ))
    })
}

/// `GET` → `{ user, sessionExpiresAt }` for the presented session.
pub fn me(ctx: &mut HandlerContext, req: Request, _opts: &AuthHttpOptions) -> Response {
    route(|| {
        with_tx(ctx, |tx| {
            let cfg = require_config(tx)?;
            let session = request_session(tx, &cfg, &req).ok_or(herr("unauthenticated", 401))?;
            let user = tx
                .db
                .auth_user()
                .user_id()
                .find(&session.user_id)
                .ok_or(herr("unauthenticated", 401))?;
            Ok(json_response(
                &serde_json::json!({
                    "user": public_user(&user),
                    "sessionExpiresAt": now_micros(session.expires_at).div_euclid(ONE_SECOND_MICROS),
                }),
                200,
                &[],
            ))
        })
    })
}

/// Replaces the presented session; connections bound to it move to the new one.
pub fn refresh(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    let ip = client_key(&req, opts.trusted_proxy_header);
    let ua = user_agent(&req);
    route(|| {
        with_tx(ctx, |tx| {
            let cfg = require_config(tx)?;
            let old = request_session(tx, &cfg, &req).ok_or(herr("unauthenticated", 401))?;
            let user = tx
                .db
                .auth_user()
                .user_id()
                .find(&old.user_id)
                .ok_or(herr("unauthenticated", 401))?;
            let next = issue_session(tx, &cfg, &user.user_id, ip.clone(), ua.clone(), opts)
                .map_err(internal)?;
            let bindings: Vec<_> = tx
                .db
                .auth_connection_binding()
                .session_id()
                .filter(old.session_id.as_str())
                .collect();
            for b in bindings {
                tx.db
                    .auth_connection_binding()
                    .stdb_identity()
                    .update(AuthConnectionBinding {
                        session_id: next.session_id.clone(),
                        ..b
                    });
            }
            tx.db.auth_session().session_id().delete(&old.session_id);
            Ok(json_response(
                &serde_json::json!({ "user": public_user(&user), "token": next.token, "sessionExpiresAt": next.expires_at }),
                200,
                &[next.cookie],
            ))
        })
    })
}

/// Deletes the presented session (if any) and clears the cookie.
pub fn logout(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    route(|| {
        let cookie_name = with_tx(ctx, |tx| {
            let cfg = require_config(tx)?;
            if let Some(s) = request_session(tx, &cfg, &req) {
                delete_session(tx, &s);
            }
            Ok(cfg.cookie_name)
        })?;
        Ok(json_response(
            &serde_json::json!({ "ok": true }),
            200,
            &[clear_cookie(&cookie_name, opts.secure_cookies)],
        ))
    })
}

// ---------------------------------------------------------------------------
// Email verification and password reset
// ---------------------------------------------------------------------------

fn build_mail(
    kind: &str,
    to: &str,
    base_url: &str,
    token: &str,
    app_name: Option<&str>,
) -> MailParams {
    let app = app_name.unwrap_or("this app");
    let enc = encode_uri_component(token);
    if kind == "verify" {
        let url = format!("{base_url}/auth/email/verify?token={enc}");
        MailParams {
            to: to.into(),
            subject: format!("Verify your email for {app}"),
            text: format!("Click to verify your email:\n\n{url}\n\nThis link expires in 24 hours."),
            html: Some(format!(
                r#"<p>Click to verify your email:</p><p><a href="{url}">{url}</a></p><p>This link expires in 24 hours.</p>"#
            )),
        }
    } else {
        let url = format!("{base_url}/auth/password/reset?token={enc}");
        MailParams {
            to: to.into(),
            subject: format!("Reset your password for {app}"),
            text: format!(
                "Click to reset your password:\n\n{url}\n\nThis link expires in 1 hour. Ignore this email if the request was unexpected."
            ),
            html: Some(format!(
                r#"<p>Click to reset your password:</p><p><a href="{url}">{url}</a></p><p>This link expires in 1 hour. Ignore this email if the request was unexpected.</p>"#
            )),
        }
    }
}

/// JS `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn find_verification(tx: &ReducerContext, token: &str, purpose: &str) -> HResult<AuthVerification> {
    match tx.db.auth_verification().value().find(token.to_string()) {
        Some(row) if row.purpose == purpose && row.expires_at >= tx.timestamp => Ok(row),
        _ => Err(herr("bad_token", 400)),
    }
}

/// Replaces any pending token for this purpose and returns a new one.
fn new_verification(
    tx: &ReducerContext,
    email: &str,
    purpose: &str,
    ttl_seconds: i64,
) -> Result<String, String> {
    let old: Vec<_> = tx
        .db
        .auth_verification()
        .identifier()
        .filter(email)
        .filter(|r| r.purpose == purpose)
        .collect();
    for r in old {
        tx.db
            .auth_verification()
            .verification_id()
            .delete(&r.verification_id);
    }
    let value = new_token(tx)?;
    tx.db.auth_verification().insert(AuthVerification {
        verification_id: new_id(tx)?,
        identifier: email.into(),
        value: value.clone(),
        purpose: purpose.into(),
        expires_at: plus_seconds(tx.timestamp, ttl_seconds),
        created_at: tx.timestamp,
    });
    Ok(value)
}

/// `POST` `{ email }`: mails a reset link if the account exists. Always 200.
pub fn forgot_password(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    let ip = client_key(&req, opts.trusted_proxy_header);
    route(|| {
        let send_mail = opts.send_mail.ok_or(herr("mailer_not_configured", 500))?;
        let body = body_json(req).ok_or(herr("invalid_request", 400))?;
        let email = str_field(&body, "email")
            .map_err(|_| herr("invalid_request", 400))?
            .filter(|s| !s.is_empty())
            .ok_or(herr("invalid_request", 400))?;
        let email = normalize_email(&email);
        if email.is_empty() || utf16_len(&email) > MAX_EMAIL_LEN {
            return Err(herr("invalid_request", 400));
        }
        let mut checks = Vec::new();
        if let Some(ip) = &ip {
            checks.push((limits::PASSWORD_FORGOT_IP, format!("ip:{ip}")));
        }
        checks.push((limits::PASSWORD_FORGOT_EMAIL, format!("email:{email}")));
        if let Some(r) = enforce_rate_limits(ctx, &checks)? {
            return Ok(r);
        }
        let mail = with_tx(ctx, |tx| {
            let cfg = require_config(tx)?;
            if tx.db.auth_user().email().find(&email).is_none() {
                return Ok(None);
            }
            let token = new_verification(tx, &email, "password_reset", RESET_TOKEN_TTL_SECONDS)
                .map_err(internal)?;
            Ok(Some(build_mail(
                "reset",
                &email,
                &cfg.base_url,
                &token,
                opts.app_name,
            )))
        })?;
        if let Some(mail) = mail {
            send_mail(ctx, &mail);
        }
        Ok(json_response(&serde_json::json!({ "ok": true }), 200, &[]))
    })
}

/// `POST` `{ token, newPassword }`: sets the password, verifies the address, revokes every session.
pub fn reset_password(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    let ip_req = client_key(&req, opts.trusted_proxy_header);
    route(|| {
        let body = body_json(req).ok_or(herr("invalid_request", 400))?;
        let bad = || herr("invalid_request", 400);
        let token = str_field(&body, "token")
            .map_err(|_| bad())?
            .filter(|s| !s.is_empty())
            .ok_or_else(bad)?;
        let new_password = str_field(&body, "newPassword")
            .map_err(|_| bad())?
            .filter(|s| !s.is_empty())
            .ok_or_else(bad)?;
        if utf16_len(&new_password) < MIN_PASSWORD_LEN {
            return Err(herr("password_too_short", 400));
        }
        if utf16_len(&new_password) > MAX_PASSWORD_LEN {
            return Err(herr("password_too_long", 400));
        }
        if utf16_len(&token) > MAX_TOKEN_LEN {
            return Err(herr("bad_token", 400));
        }
        if let Some(ip) = ip_req
            && let Some(r) =
                enforce_rate_limits(ctx, &[(limits::PASSWORD_RESET, format!("ip:{ip}"))])?
        {
            return Ok(r);
        }
        // Check the token before the slow hash.
        let salt = with_tx(ctx, |tx| {
            find_verification(tx, &token, "password_reset")?;
            new_salt(tx).map_err(internal)
        })?;
        let password_hash = hash_password(&new_password, &salt);
        with_tx(ctx, |tx| {
            let row = find_verification(tx, &token, "password_reset")?;
            tx.db
                .auth_verification()
                .verification_id()
                .delete(&row.verification_id);
            let user = tx
                .db
                .auth_user()
                .email()
                .find(&row.identifier)
                .ok_or(herr("bad_token", 400))?;
            match password_account(tx, &user.user_id) {
                Some(acct) => {
                    tx.db.auth_account().account_id().update(AuthAccount {
                        password_hash: Some(password_hash.clone()),
                        updated_at: tx.timestamp,
                        ..acct
                    });
                }
                None => {
                    tx.db.auth_account().insert(AuthAccount {
                        account_id: new_id(tx).map_err(internal)?,
                        user_id: user.user_id.clone(),
                        provider_id: "password".into(),
                        provider_account_id: user.email.clone(),
                        password_hash: Some(password_hash.clone()),
                        access_token: None,
                        refresh_token: None,
                        access_token_expires_at: None,
                        created_at: tx.timestamp,
                        updated_at: tx.timestamp,
                    });
                }
            }
            // The reset link proved control of the mailbox.
            if !user.email_verified {
                tx.db.auth_user().user_id().update(AuthUser {
                    email_verified: true,
                    updated_at: tx.timestamp,
                    ..user.clone()
                });
            }
            let sessions: Vec<_> = tx
                .db
                .auth_session()
                .user_id()
                .filter(user.user_id.as_str())
                .collect();
            for s in sessions {
                delete_session(tx, &s);
            }
            Ok(())
        })?;
        Ok(json_response(&serde_json::json!({ "ok": true }), 200, &[]))
    })
}

/// `POST` with a session: mails a verification link (or reports `alreadyVerified`).
pub fn email_verify_request(
    ctx: &mut HandlerContext,
    req: Request,
    opts: &AuthHttpOptions,
) -> Response {
    route(|| {
        let send_mail = opts.send_mail.ok_or(herr("mailer_not_configured", 500))?;
        if let Some(r) = enforce_ip_rate_limit(
            ctx,
            &req,
            limits::EMAIL_VERIFY_REQUEST,
            opts.trusted_proxy_header,
        )? {
            return Ok(r);
        }
        let mail = with_tx(ctx, |tx| {
            let cfg = require_config(tx)?;
            let session = request_session(tx, &cfg, &req).ok_or(herr("unauthenticated", 401))?;
            let user = tx
                .db
                .auth_user()
                .user_id()
                .find(&session.user_id)
                .ok_or(herr("unauthenticated", 401))?;
            if user.email_verified {
                return Ok(None);
            }
            let token = new_verification(tx, &user.email, "email_verify", VERIFY_TOKEN_TTL_SECONDS)
                .map_err(internal)?;
            Ok(Some(build_mail(
                "verify",
                &user.email,
                &cfg.base_url,
                &token,
                opts.app_name,
            )))
        })?;
        let Some(mail) = mail else {
            return Ok(json_response(
                &serde_json::json!({ "ok": true, "alreadyVerified": true }),
                200,
                &[],
            ));
        };
        send_mail(ctx, &mail);
        Ok(json_response(&serde_json::json!({ "ok": true }), 200, &[]))
    })
}

/// `GET ?token=`: marks the address verified and redirects.
pub fn email_verify(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    route(|| {
        let token = query_param(&req, "token")
            .filter(|t| !t.is_empty())
            .ok_or(herr("missing_token", 400))?;
        with_tx(ctx, |tx| {
            let row = find_verification(tx, &token, "email_verify")?;
            tx.db
                .auth_verification()
                .verification_id()
                .delete(&row.verification_id);
            let user = tx
                .db
                .auth_user()
                .email()
                .find(&row.identifier)
                .ok_or(herr("bad_token", 400))?;
            tx.db.auth_user().user_id().update(AuthUser {
                email_verified: true,
                updated_at: tx.timestamp,
                ..user
            });
            Ok(())
        })?;
        Ok(redirect_response(
            opts.email_verified_redirect.unwrap_or("/"),
            &[],
        ))
    })
}

// ---------------------------------------------------------------------------
// OAuth
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthProfile {
    pub sub: String,
    pub email: String,
    pub email_verified: bool,
    pub name: Option<String>,
    pub image: Option<String>,
}

/// Fetches a profile with the access token, for providers that need more than one request.
pub type ResolveProfileFn =
    fn(&mut HandlerContext, &OAuthProviderSpec, &str) -> Result<OAuthProfile, String>;

/// A sign-in provider. `resolve_profile` (if set) replaces the single userinfo request.
#[derive(Clone, Copy)]
pub struct OAuthProviderSpec {
    pub id: &'static str,
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub scope: &'static str,
    pub client_id: fn(&AuthConfig) -> String,
    pub client_secret: fn(&AuthConfig) -> String,
    /// Reserved for verified OIDC id_token support. Prefer `user_info_url`.
    pub oidc: bool,
    pub user_info_url: Option<&'static str>,
    pub user_info_headers: &'static [(&'static str, &'static str)],
    pub parse_profile: fn(&Value) -> OAuthProfile,
    pub resolve_profile: Option<ResolveProfileFn>,
    pub authorize_extras: &'static [(&'static str, &'static str)],
    /// Default true.
    pub use_pkce: bool,
}

fn json_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// JS `String(value ?? '')`.
fn js_string_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => js_number_string(n),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| js_string_of(Some(x)))
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

fn js_number_string(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{f:.0}")
    } else {
        f.to_string()
    }
}

fn google_profile(claims: &Value) -> OAuthProfile {
    OAuthProfile {
        sub: js_string_of(claims.get("sub")),
        email: js_string_of(claims.get("email")),
        email_verified: claims.get("email_verified") == Some(&Value::Bool(true)),
        name: json_str(claims, "name"),
        image: json_str(claims, "picture"),
    }
}

fn github_profile(user: &Value) -> OAuthProfile {
    OAuthProfile {
        sub: js_string_of(user.get("id")),
        email: js_string_of(user.get("email")),
        email_verified: true,
        name: json_str(user, "name").or_else(|| Some(js_string_of(user.get("login")))),
        image: json_str(user, "avatar_url"),
    }
}

const GITHUB_HEADERS: &[(&str, &str)] = &[
    ("accept", "application/vnd.github+json"),
    ("x-github-api-version", "2022-11-28"),
    ("user-agent", "spacetimedb-auth-submodule"),
];

fn pick_github_email(rows: &Value) -> String {
    let Some(rows) = rows.as_array() else {
        return String::new();
    };
    let is = |r: &Value, k: &str| r.get(k) == Some(&Value::Bool(true));
    let email = |r: &Value| r.get("email").and_then(Value::as_str).map(str::to_string);
    rows.iter()
        .find(|r| is(r, "primary") && is(r, "verified") && email(r).is_some())
        .or_else(|| {
            rows.iter()
                .find(|r| is(r, "verified") && email(r).is_some())
        })
        .and_then(email)
        .unwrap_or_default()
}

fn http_get_json(
    ctx: &mut HandlerContext,
    url: &str,
    headers: &[(&str, String)],
) -> Result<(u16, Option<Value>), String> {
    let mut b = http::Request::builder().method("GET").uri(url);
    for (k, v) in headers {
        b = b.header(*k, v.as_str());
    }
    let req = b.body(Body::empty()).map_err(|e| e.to_string())?;
    let res = ctx.http.send(req).map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let text = res.into_body().into_string_lossy();
    Ok((status, serde_json::from_str(&text).ok()))
}

/// GitHub: `/user`, then a verified address from `/user/emails`, both under `user_info_url`.
pub fn resolve_github_profile(
    ctx: &mut HandlerContext,
    spec: &OAuthProviderSpec,
    access_token: &str,
) -> Result<OAuthProfile, String> {
    let user_url = spec.user_info_url.unwrap_or("https://api.github.com/user");
    let mut headers: Vec<(&str, String)> = GITHUB_HEADERS
        .iter()
        .map(|(k, v)| (*k, v.to_string()))
        .collect();
    headers.push(("authorization", format!("Bearer {access_token}")));
    let (status, user) =
        http_get_json(ctx, user_url, &headers).map_err(|_| "userinfo_failed:0".to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("userinfo_failed:{status}"));
    }
    let user = user.ok_or("incomplete_profile")?;
    let (status, emails) = http_get_json(ctx, &format!("{user_url}/emails"), &headers)
        .map_err(|_| "github_email_failed:0".to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("github_email_failed:{status}"));
    }
    let email = pick_github_email(&emails.unwrap_or(Value::Null));
    Ok(OAuthProfile {
        sub: js_string_of(user.get("id")),
        email_verified: !email.is_empty(),
        email,
        name: json_str(&user, "name").or_else(|| Some(js_string_of(user.get("login")))),
        image: json_str(&user, "avatar_url"),
    })
}

pub const GOOGLE: OAuthProviderSpec = OAuthProviderSpec {
    id: "google",
    authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scope: "openid email profile",
    client_id: |c| c.google_client_id.clone().unwrap_or_default(),
    client_secret: |c| c.google_client_secret.clone().unwrap_or_default(),
    oidc: false,
    user_info_url: Some("https://openidconnect.googleapis.com/v1/userinfo"),
    user_info_headers: &[],
    parse_profile: google_profile,
    resolve_profile: None,
    authorize_extras: &[("access_type", "offline"), ("prompt", "consent")],
    use_pkce: true,
};

pub const GITHUB: OAuthProviderSpec = OAuthProviderSpec {
    id: "github",
    authorize_url: "https://github.com/login/oauth/authorize",
    token_url: "https://github.com/login/oauth/access_token",
    scope: "read:user user:email",
    client_id: |c| c.github_client_id.clone().unwrap_or_default(),
    client_secret: |c| c.github_client_secret.clone().unwrap_or_default(),
    oidc: false,
    user_info_url: Some("https://api.github.com/user"),
    user_info_headers: GITHUB_HEADERS,
    parse_profile: github_profile,
    resolve_profile: Some(resolve_github_profile),
    authorize_extras: &[],
    use_pkce: false,
};

/// Google's profile parser, for custom Google-shaped providers.
pub fn parse_google_profile(v: &Value) -> OAuthProfile {
    google_profile(v)
}

fn state_cookie_name(cfg: &AuthConfig) -> String {
    format!("{}_oauth_state", cfg.cookie_name)
}

fn callback_url(cfg: &AuthConfig, provider: &OAuthProviderSpec) -> String {
    format!("{}/auth/{}/callback", cfg.base_url, provider.id)
}

fn form_encode(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", encode_uri_component(k), encode_uri_component(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `GET ?redirectTo=`: stores state (and a PKCE verifier) and redirects to the provider.
pub fn oauth_start(
    ctx: &mut HandlerContext,
    req: Request,
    opts: &AuthHttpOptions,
    provider: &OAuthProviderSpec,
) -> Response {
    route(|| {
        let redirect_to = match query_param(&req, "redirectTo") {
            None => Some("/".to_string()),
            Some(r) => safe_redirect_path(&r),
        }
        .ok_or(herr("invalid_redirect", 400))?;
        if let Some(ip) = client_key(&req, opts.trusted_proxy_header)
            && let Some(r) = enforce_rate_limits(
                ctx,
                &[(limits::OAUTH_START, format!("ip:{ip}:{}", provider.id))],
            )?
        {
            return Ok(r);
        }
        let (cfg, client_id, state, verifier) = with_tx(ctx, |tx| {
            let cfg = require_config(tx)?;
            let client_id = (provider.client_id)(&cfg);
            if client_id.is_empty() {
                return Err(herr(
                    format!("provider_not_configured:{}", provider.id),
                    500,
                ));
            }
            let state = new_token(tx).map_err(internal)?;
            let verifier = if provider.use_pkce {
                new_token(tx).map_err(internal)?
            } else {
                String::new()
            };
            tx.db.auth_oauth_state().insert(AuthOauthState {
                state: state.clone(),
                provider: provider.id.into(),
                code_verifier: verifier.clone(),
                redirect_to: redirect_to.clone(),
                expires_at: plus_seconds(tx.timestamp, OAUTH_STATE_TTL_SECONDS),
                created_at: tx.timestamp,
            });
            Ok((cfg, client_id, state, verifier))
        })?;
        let mut params: Vec<(String, String)> = vec![
            ("client_id".into(), client_id),
            ("redirect_uri".into(), callback_url(&cfg, provider)),
            ("response_type".into(), "code".into()),
            ("scope".into(), provider.scope.into()),
            ("state".into(), state.clone()),
        ];
        if provider.use_pkce {
            params.push(("code_challenge".into(), pkce_challenge(&verifier)));
            params.push(("code_challenge_method".into(), "S256".into()));
        }
        for (k, v) in provider.authorize_extras {
            match params.iter_mut().find(|(pk, _)| pk == k) {
                Some(p) => p.1 = v.to_string(),
                None => params.push((k.to_string(), v.to_string())),
            }
        }
        let sep = if provider.authorize_url.contains('?') {
            '&'
        } else {
            '?'
        };
        Ok(redirect_response(
            &format!("{}{sep}{}", provider.authorize_url, form_encode(&params)),
            &[make_cookie(
                &state_cookie_name(&cfg),
                &state,
                Some(OAUTH_STATE_TTL_SECONDS),
                opts.secure_cookies,
            )],
        ))
    })
}

/// `GET ?code=&state=`: checks state (and its browser cookie), exchanges the code, finds or
/// creates the user, issues a session and redirects to the stored path.
pub fn oauth_callback(
    ctx: &mut HandlerContext,
    req: Request,
    opts: &AuthHttpOptions,
    provider: &OAuthProviderSpec,
) -> Response {
    let ip = client_key(&req, opts.trusted_proxy_header);
    let ua = user_agent(&req);
    route(|| {
        let code = query_param(&req, "code").filter(|c| !c.is_empty());
        let state = query_param(&req, "state").filter(|s| !s.is_empty());
        let (Some(code), Some(state)) = (code, state) else {
            return Err(herr("missing_code_or_state", 400));
        };
        if utf16_len(&code) > MAX_OAUTH_CODE_LENGTH || utf16_len(&state) > MAX_OAUTH_STATE_LENGTH {
            return Err(herr("invalid_code_or_state", 400));
        }
        let (cfg, code_verifier, redirect_to) = with_tx(ctx, |tx| {
            let cfg = require_config(tx)?;
            let row = tx.db.auth_oauth_state().state().find(&state);
            let cookie_state = cookie(&req, &state_cookie_name(&cfg));
            let row = match row {
                Some(r)
                    if r.provider == provider.id
                        && r.expires_at >= tx.timestamp
                        && cookie_state.as_deref() == Some(state.as_str()) =>
                {
                    r
                }
                _ => return Err(herr("bad_state", 400)),
            };
            tx.db.auth_oauth_state().state().delete(&row.state);
            Ok((cfg, row.code_verifier, row.redirect_to))
        })?;
        let mut form: Vec<(String, String)> = vec![
            ("grant_type".into(), "authorization_code".into()),
            ("code".into(), code.clone()),
            ("client_id".into(), (provider.client_id)(&cfg)),
            ("client_secret".into(), (provider.client_secret)(&cfg)),
            ("redirect_uri".into(), callback_url(&cfg, provider)),
        ];
        if !code_verifier.is_empty() {
            form.push(("code_verifier".into(), code_verifier));
        }
        let token_req = http::Request::builder()
            .method("POST")
            .uri(provider.token_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(form_encode(&form))
            .map_err(|_| herr("token_exchange_failed", 502))?;
        let tok_res = ctx
            .http
            .send(token_req)
            .map_err(|_| herr("token_exchange_failed", 502))?;
        if !tok_res.status().is_success() {
            return Err(herr("token_exchange_failed", 502));
        }
        let tokens: Value =
            serde_json::from_str(&tok_res.into_body().into_string_lossy()).unwrap_or(Value::Null);
        let access_token = json_str(&tokens, "access_token");
        let refresh_token = json_str(&tokens, "refresh_token");
        let id_token = json_str(&tokens, "id_token");
        let expires_in = tokens
            .get("expires_in")
            .and_then(Value::as_f64)
            .filter(|e| e.fract() == 0.0 && *e > 0.0 && *e <= 9_007_199_254_740_991.0)
            .map(|e| e as i64);
        if access_token.is_none() && id_token.is_none() {
            return Err(herr("no_token_in_response", 502));
        }
        let profile = if let (Some(resolve), Some(at)) =
            (provider.resolve_profile, access_token.as_deref())
        {
            resolve(ctx, provider, at).map_err(|e| herr(e, 502))?
        } else if let (Some(url), Some(at)) = (provider.user_info_url, access_token.as_deref()) {
            let mut headers: Vec<(&str, String)> = vec![
                ("authorization", format!("Bearer {at}")),
                ("accept", "application/json".into()),
                ("user-agent", "spacetimedb-auth-submodule".into()),
            ];
            for (k, v) in provider.user_info_headers {
                headers.retain(|(hk, _)| !hk.eq_ignore_ascii_case(k));
                headers.push((k, v.to_string()));
            }
            let (status, data) =
                http_get_json(ctx, url, &headers).map_err(|_| herr("userinfo_failed:0", 502))?;
            if !(200..300).contains(&status) {
                return Err(herr(format!("userinfo_failed:{status}"), 502));
            }
            (provider.parse_profile)(&data.unwrap_or(Value::Null))
        } else if provider.oidc && id_token.is_some() {
            return Err(herr("id_token_verification_unsupported", 502));
        } else {
            return Err(herr("cannot_resolve_profile", 502));
        };
        if profile.email.is_empty()
            || profile.sub.is_empty()
            || utf16_len(&profile.email) > MAX_PROFILE_EMAIL_LENGTH
            || utf16_len(&profile.sub) > MAX_PROFILE_SUB_LENGTH
            || profile.name.as_deref().map_or(0, utf16_len) > MAX_OAUTH_PROFILE_NAME_LENGTH
            || profile.image.as_deref().map_or(0, utf16_len) > MAX_OAUTH_PROFILE_IMAGE_LENGTH
        {
            return Err(herr("incomplete_profile", 502));
        }
        let email = profile.email.to_lowercase();
        let session = with_tx(ctx, |tx| {
            let cfg = require_config(tx)?;
            let expires_at = expires_in.map(|e| plus_seconds(tx.timestamp, e));
            let existing = tx
                .db
                .auth_account()
                .provider_account_id()
                .filter(profile.sub.as_str())
                .find(|a| a.provider_id == provider.id);
            let user_id = match existing {
                Some(acct) => {
                    let uid = acct.user_id.clone();
                    tx.db.auth_account().account_id().update(AuthAccount {
                        access_token: access_token.clone().or(acct.access_token.clone()),
                        refresh_token: refresh_token.clone().or(acct.refresh_token.clone()),
                        access_token_expires_at: expires_at.or(acct.access_token_expires_at),
                        updated_at: tx.timestamp,
                        ..acct
                    });
                    uid
                }
                None => {
                    let uid = match tx.db.auth_user().email().find(&email) {
                        // Link only when both sides proved control of the address.
                        Some(by_email) => {
                            if !profile.email_verified || !by_email.email_verified {
                                return Err(herr("account_link_required", 409));
                            }
                            by_email.user_id
                        }
                        None => {
                            let uid = new_id(tx).map_err(internal)?;
                            tx.db.auth_user().insert(AuthUser {
                                user_id: uid.clone(),
                                email: email.clone(),
                                email_verified: profile.email_verified,
                                name: profile.name.clone(),
                                image: profile.image.clone(),
                                created_at: tx.timestamp,
                                updated_at: tx.timestamp,
                            });
                            uid
                        }
                    };
                    tx.db.auth_account().insert(AuthAccount {
                        account_id: new_id(tx).map_err(internal)?,
                        user_id: uid.clone(),
                        provider_id: provider.id.into(),
                        provider_account_id: profile.sub.clone(),
                        password_hash: None,
                        access_token: access_token.clone(),
                        refresh_token: refresh_token.clone(),
                        access_token_expires_at: expires_at,
                        created_at: tx.timestamp,
                        updated_at: tx.timestamp,
                    });
                    uid
                }
            };
            issue_session(tx, &cfg, &user_id, ip.clone(), ua.clone(), opts).map_err(internal)
        })?;
        Ok(redirect_response(
            &redirect_to,
            &[
                session.cookie,
                clear_cookie(&state_cookie_name(&cfg), opts.secure_cookies),
            ],
        ))
    })
}

pub fn google_start(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    oauth_start(ctx, req, opts, &GOOGLE)
}
pub fn google_callback(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    oauth_callback(ctx, req, opts, &GOOGLE)
}
pub fn github_start(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    oauth_start(ctx, req, opts, &GITHUB)
}
pub fn github_callback(ctx: &mut HandlerContext, req: Request, opts: &AuthHttpOptions) -> Response {
    oauth_callback(ctx, req, opts, &GITHUB)
}
