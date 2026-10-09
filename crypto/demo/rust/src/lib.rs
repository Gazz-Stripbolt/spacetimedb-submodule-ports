//! Demo for crypto.rs: each helper behind a procedure, so the shared test suite can drive it
//! over HTTP and compare the Rust and C# ports byte for byte.

#[path = "../../../rust/crypto.rs"]
pub mod crypto;

use spacetimedb::{ProcedureContext, procedure};

fn hex(s: &str) -> Result<Vec<u8>, String> {
    crypto::hex_to_bytes(s)
}

/// SHA-256 of hex input, as hex. `repeat` repeats the input (for the 1,000,000 × "a" vector).
#[procedure]
pub fn crypto_sha256(
    _ctx: &mut ProcedureContext,
    data_hex: String,
    repeat: u32,
) -> Result<String, String> {
    let data = hex(&data_hex)?.repeat(repeat.max(1) as usize);
    Ok(crypto::bytes_to_hex(&crypto::sha256(&data)))
}

#[procedure]
pub fn crypto_hmac_sha256(
    _ctx: &mut ProcedureContext,
    key_hex: String,
    message_hex: String,
) -> Result<String, String> {
    Ok(crypto::bytes_to_hex(&crypto::hmac_sha256(
        &hex(&key_hex)?,
        &hex(&message_hex)?,
    )))
}

#[procedure]
pub fn crypto_timing_safe_equal(
    _ctx: &mut ProcedureContext,
    a_hex: String,
    b_hex: String,
) -> Result<bool, String> {
    Ok(crypto::timing_safe_equal(&hex(&a_hex)?, &hex(&b_hex)?))
}

/// Decode base64 to hex, or the error message.
#[procedure]
pub fn crypto_base64_to_hex(_ctx: &mut ProcedureContext, b64: String) -> Result<String, String> {
    crypto::base64_to_bytes(&b64).map(|b| crypto::bytes_to_hex(&b))
}

/// Decode hex and re-encode it (lowercase), or the error message.
#[procedure]
pub fn crypto_hex_roundtrip(_ctx: &mut ProcedureContext, hex_in: String) -> Result<String, String> {
    crypto::hex_to_bytes(&hex_in).map(|b| crypto::bytes_to_hex(&b))
}

fn outcome(r: crypto::VerifyResult) -> String {
    match r {
        Ok(()) => "ok".into(),
        Err(e) => e.code().into(),
    }
}

#[procedure]
pub fn crypto_verify_stripe(
    _ctx: &mut ProcedureContext,
    raw_body: String,
    signature_header: String,
    secret: String,
    tolerance_seconds: Option<u64>,
    now_seconds: i64,
) -> String {
    outcome(crypto::verify_stripe_signature(&crypto::StripeVerifyOpts {
        raw_body: &raw_body,
        signature_header: &signature_header,
        secret: &secret,
        tolerance_seconds,
        now_seconds,
    }))
}

#[procedure]
#[allow(clippy::too_many_arguments)]
pub fn crypto_verify_svix(
    _ctx: &mut ProcedureContext,
    raw_body: String,
    svix_id: String,
    svix_timestamp: String,
    svix_signature: String,
    secret: String,
    tolerance_seconds: Option<u64>,
    now_seconds: i64,
) -> String {
    outcome(crypto::verify_svix_signature(&crypto::SvixVerifyOpts {
        raw_body: &raw_body,
        svix_id: &svix_id,
        svix_timestamp: &svix_timestamp,
        svix_signature: &svix_signature,
        secret: &secret,
        tolerance_seconds,
        now_seconds,
    }))
}

#[procedure]
pub fn crypto_verify_github(
    _ctx: &mut ProcedureContext,
    raw_body: String,
    signature_header: String,
    secret: String,
) -> String {
    outcome(crypto::verify_github_signature(&crypto::GithubVerifyOpts {
        raw_body: &raw_body,
        signature_header: &signature_header,
        secret: &secret,
    }))
}
