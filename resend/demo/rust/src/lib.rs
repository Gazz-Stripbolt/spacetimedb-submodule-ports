//! Demo for resend.rs: the standalone package's procedures and reducers, the webhook route at
//! `/webhook/resend`, and the upstream example's pattern of a host send procedure plus a
//! caller-scoped history view.
//!
//! Build with `RESEND_API_BASE=http://127.0.0.1:4201` to send to the test suite's mock API.

#[path = "../../../../crypto/rust/crypto.rs"]
pub mod crypto;
#[path = "../../../rust/resend.rs"]
pub mod resend;

use resend::{ResendEmail, resend_email as _};
use spacetimedb::http::{Router, router};
use spacetimedb::{ProcedureContext, ReducerContext, Table};

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    resend::install(ctx);
}

#[router]
fn routes() -> Router {
    Router::new().post("/webhook/resend", resend::resend_webhook)
}

/// The host decides who may send: here anyone may email themselves a welcome note (the address
/// is checked and the history is tagged with the caller), like the upstream `sendDispatch`.
#[spacetimedb::procedure]
pub fn send_welcome(
    ctx: &mut ProcedureContext,
    to: String,
) -> Result<resend::ResendSendEmailResult, String> {
    let to = to.trim().to_lowercase();
    if to.split('@').count() != 2 || to.chars().any(char::is_whitespace) {
        return Err("dispatch.invalid_email".into());
    }
    let user_id = ctx.sender().to_hex().to_string();
    resend::send_email_request(
        ctx,
        resend::SendEmailArgs {
            to: vec![to],
            subject: "Welcome".into(),
            html: Some("<p>Your workspace is ready.</p>".into()),
            tags_json: Some(format!(r#"[{{"name":"userId","value":"{user_id}"}}]"#)),
            ..Default::default()
        },
    )
}

/// The caller's own emails (Resend's tables stay private). A procedure rather than the
/// upstream example's view: Rust views can only read indexes, and Rust can't filter the
/// `Option`-typed `user_id` index.
#[spacetimedb::procedure]
pub fn my_emails(ctx: &mut ProcedureContext) -> Vec<ResendEmail> {
    let me = Some(ctx.sender().to_hex().to_string());
    ctx.with_tx(|tx| {
        tx.db
            .resend_email()
            .iter()
            .filter(|e| e.user_id == me)
            .collect()
    })
}
