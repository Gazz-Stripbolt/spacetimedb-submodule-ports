//! Demo for api_keys.rs: the upstream example module (install + the client operations) plus
//! procedures the test suite uses to verify keys and drive the host helpers.

#[path = "../../../rust/api_keys.rs"]
pub mod api_keys;
#[path = "../../../../crypto/rust/crypto.rs"]
pub mod crypto;

use api_keys::{ApiKeyUsage, ApiKeyVerifyResult, VerifyApiKeyArgs, api_key_usage};
use spacetimedb::{ProcedureContext, ReducerContext, Timestamp};

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    api_keys::install(ctx);
}

/// What an HTTP handler or procedure receiving a key would do.
#[spacetimedb::procedure]
pub fn demo_verify(
    ctx: &mut ProcedureContext,
    key: String,
    required_scope: Option<String>,
    action: Option<String>,
) -> Result<ApiKeyVerifyResult, String> {
    ctx.try_with_tx(|tx| {
        api_keys::verify_api_key(
            tx,
            &VerifyApiKeyArgs {
                key: key.clone(),
                required_scope: required_scope.clone(),
                action: action.clone(),
            },
        )
    })
}

#[spacetimedb::procedure]
pub fn demo_has_scope(
    _ctx: &mut ProcedureContext,
    scopes_json: String,
    required: Option<String>,
) -> bool {
    api_keys::has_scope(&scopes_json, required.as_deref())
}

/// `JSON.stringify(JSON.parse(text))`, or the error.
#[spacetimedb::procedure]
pub fn demo_json(_ctx: &mut ProcedureContext, text: String) -> Result<String, String> {
    api_keys::json::parse(&text)
        .map(|v| api_keys::json::stringify(&v))
        .ok_or_else(|| "invalid".into())
}

/// The derived key for a secret, counter and timestamp (cross-language vector check).
#[spacetimedb::procedure]
pub fn demo_derive(
    _ctx: &mut ProcedureContext,
    secret: String,
    counter: u64,
    micros: i64,
    key_prefix: String,
) -> Vec<String> {
    let (key, prefix) = api_keys::format_api_key(
        &key_prefix,
        &api_keys::derive_key_secret(&secret, counter, micros),
    );
    let hash = api_keys::hash_api_key(&key);
    vec![key, prefix, hash]
}

/// Backdate a usage row, so the retention sweep has something to delete.
#[spacetimedb::reducer]
pub fn demo_backdate_usage(
    ctx: &ReducerContext,
    usage_id: u64,
    seconds: u32,
) -> Result<(), String> {
    api_keys::require_admin(ctx)?;
    let row = ctx
        .db
        .api_key_usage()
        .usage_id()
        .find(usage_id)
        .ok_or("no such usage row")?;
    let used_at = Timestamp::from_micros_since_unix_epoch(
        row.used_at.to_micros_since_unix_epoch() - seconds as i64 * 1_000_000,
    );
    ctx.db.api_key_usage().usage_id().update(ApiKeyUsage {
        used_at,
        used_at_order: -used_at.to_micros_since_unix_epoch(),
        ..row
    });
    Ok(())
}

#[spacetimedb::procedure]
pub fn demo_run_usage_sweep(ctx: &mut ProcedureContext) -> Result<u32, String> {
    ctx.try_with_tx(|tx| {
        api_keys::require_admin(tx)?;
        Ok(api_keys::run_usage_sweep(tx))
    })
}
