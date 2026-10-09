//! Demo for auth.rs: the upstream example module (private notes behind sign-in, every auth
//! route) plus a mock OAuth provider pair and procedures the test suite uses to check the
//! port's crypto against vectors from the TS code.

#[path = "../../../rust/auth.rs"]
pub mod auth;
#[path = "../../../../crypto/rust/crypto.rs"]
pub mod crypto;
#[path = "../../../../rate-limit/rust/rate_limit.rs"]
pub mod rate_limit;

use auth::{
    AuthHttpOptions, JwtVerify, OAuthProviderSpec, VerifyJwtOptions, auth_connection_binding__view,
};
use spacetimedb::http::{HandlerContext, Request, Response, Router, handler, router};
use spacetimedb::{ProcedureContext, ReducerContext, SpacetimeType, Table, Timestamp, ViewContext};

// ---------------------------------------------------------------------------
// The upstream example: private notes
// ---------------------------------------------------------------------------

#[spacetimedb::table(accessor = note)]
#[derive(Clone)]
pub struct Note {
    #[primary_key]
    pub note_id: String,
    #[index(btree)]
    pub author_id: String,
    pub title: String,
    pub body: String,
    #[index(btree)]
    pub created_at: Timestamp,
}

/// Mail the demo "sends" (the upstream example logs it; tests read it back).
#[spacetimedb::table(accessor = demo_mail)]
pub struct DemoMail {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub to: String,
    pub subject: String,
    pub text: String,
}

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    auth::install(ctx);
}

#[spacetimedb::view(accessor = my_notes, public)]
fn my_notes(ctx: &ViewContext) -> Vec<Note> {
    match ctx
        .db
        .auth_connection_binding()
        .stdb_identity()
        .find(ctx.sender())
    {
        Some(b) => ctx
            .db
            .note()
            .author_id()
            .filter(b.user_id.as_str())
            .collect(),
        None => vec![],
    }
}

#[spacetimedb::reducer]
pub fn create_note(ctx: &ReducerContext, title: String, body: String) -> Result<(), String> {
    let user_id = auth::require_caller_user_id(ctx)?;
    let note_id = ctx.new_uuid_v7().map_err(|e| e.to_string())?.to_string();
    ctx.db.note().insert(Note {
        note_id,
        author_id: user_id,
        title,
        body,
        created_at: ctx.timestamp,
    });
    Ok(())
}

#[spacetimedb::reducer]
pub fn delete_note(ctx: &ReducerContext, note_id: String) -> Result<(), String> {
    let user_id = auth::require_caller_user_id(ctx)?;
    let row = ctx
        .db
        .note()
        .note_id()
        .find(&note_id)
        .ok_or("note.not_found")?;
    if row.author_id != user_id {
        return Err("note.not_owner".into());
    }
    ctx.db.note().note_id().delete(&note_id);
    Ok(())
}

#[spacetimedb::reducer]
pub fn update_note(
    ctx: &ReducerContext,
    note_id: String,
    title: String,
    body: String,
) -> Result<(), String> {
    let user_id = auth::require_caller_user_id(ctx)?;
    let row = ctx
        .db
        .note()
        .note_id()
        .find(&note_id)
        .ok_or("note.not_found")?;
    if row.author_id != user_id {
        return Err("note.not_owner".into());
    }
    ctx.db.note().note_id().update(Note { title, body, ..row });
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP routes
// ---------------------------------------------------------------------------

fn store_mail(ctx: &mut HandlerContext, mail: &auth::MailParams) {
    log::info!(
        "[mail] to={} subject={}\n{}",
        mail.to,
        mail.subject,
        mail.text
    );
    ctx.with_tx(|tx| {
        tx.db.demo_mail().insert(DemoMail {
            id: 0,
            to: mail.to.clone(),
            subject: mail.subject.clone(),
            text: mail.text.clone(),
        });
    });
}

/// Local plain-HTTP development: no `Secure` cookies; the tests send `x-real-ip`.
const AUTH_HTTP: AuthHttpOptions = AuthHttpOptions {
    trusted_proxy_header: Some(auth::TrustedProxyHeader::XRealIp),
    secure_cookies: false,
    send_mail: Some(store_mail),
    app_name: Some("Notes"),
    email_verified_redirect: Some("/?verified=1"),
};

/// Test doubles for Google and GitHub, served by the test suite's mock provider.
const MOCK_BASE: &str = "http://127.0.0.1:4310";
const MOCK: OAuthProviderSpec = OAuthProviderSpec {
    id: "mock",
    authorize_url: "http://127.0.0.1:4310/authorize",
    token_url: "http://127.0.0.1:4310/token",
    user_info_url: Some("http://127.0.0.1:4310/userinfo"),
    ..auth::GOOGLE
};
const MOCKGH: OAuthProviderSpec = OAuthProviderSpec {
    id: "mockgh",
    authorize_url: "http://127.0.0.1:4310/gh/authorize",
    token_url: "http://127.0.0.1:4310/gh/token",
    user_info_url: Some("http://127.0.0.1:4310/gh/user"),
    ..auth::GITHUB
};
const _: &str = MOCK_BASE;

#[handler]
fn route_auth_password_signup(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::password_signup(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_password_login(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::password_login(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_me(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::me(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_logout(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::logout(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_refresh(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::refresh(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_google_start(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::google_start(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_google_callback(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::google_callback(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_github_start(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::github_start(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_github_callback(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::github_callback(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_mock_start(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::oauth_start(ctx, req, &AUTH_HTTP, &MOCK)
}
#[handler]
fn route_auth_mock_callback(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::oauth_callback(ctx, req, &AUTH_HTTP, &MOCK)
}
#[handler]
fn route_auth_mockgh_start(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::oauth_start(ctx, req, &AUTH_HTTP, &MOCKGH)
}
#[handler]
fn route_auth_mockgh_callback(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::oauth_callback(ctx, req, &AUTH_HTTP, &MOCKGH)
}
#[handler]
fn route_auth_password_forgot(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::forgot_password(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_password_reset(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::reset_password(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_email_verify_request(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::email_verify_request(ctx, req, &AUTH_HTTP)
}
#[handler]
fn route_auth_email_verify(ctx: &mut HandlerContext, req: Request) -> Response {
    auth::email_verify(ctx, req, &AUTH_HTTP)
}
/// `GET /whoami-http`: the user id behind the request's bearer token or cookie (`requestUserId`).
#[handler]
fn route_whoami_http(ctx: &mut HandlerContext, req: Request) -> Response {
    let user = ctx.with_tx(|tx| auth::request_user_id(tx, &req));
    auth::json_response(&serde_json::json!({ "userId": user }), 200, &[])
}

#[router]
fn routes() -> Router {
    Router::new()
        .post("/auth/password/signup", route_auth_password_signup)
        .post("/auth/password/login", route_auth_password_login)
        .post("/auth/session/refresh", route_auth_refresh)
        .get("/auth/me", route_auth_me)
        .post("/auth/logout", route_auth_logout)
        .get("/auth/google/start", route_auth_google_start)
        .get("/auth/google/callback", route_auth_google_callback)
        .get("/auth/github/start", route_auth_github_start)
        .get("/auth/github/callback", route_auth_github_callback)
        .get("/auth/mock/start", route_auth_mock_start)
        .get("/auth/mock/callback", route_auth_mock_callback)
        .get("/auth/mockgh/start", route_auth_mockgh_start)
        .get("/auth/mockgh/callback", route_auth_mockgh_callback)
        .post("/auth/password/forgot", route_auth_password_forgot)
        .post("/auth/password/reset", route_auth_password_reset)
        .post(
            "/auth/email/verify-request",
            route_auth_email_verify_request,
        )
        .get("/auth/email/verify", route_auth_email_verify)
        .get("/whoami-http", route_whoami_http)
}

// ---------------------------------------------------------------------------
// Vector checks (pure functions)
// ---------------------------------------------------------------------------

#[derive(SpacetimeType)]
pub struct DemoKeyInfo {
    pub public_hex: String,
    pub private_key_pem: String,
    pub public_key_pem: String,
    pub kid: String,
}

fn unhex(h: &str) -> Result<Vec<u8>, String> {
    crypto::hex_to_bytes(h)
}

#[spacetimedb::procedure]
pub fn demo_hash_password(
    _ctx: &mut ProcedureContext,
    password: String,
    salt_hex: String,
) -> Result<String, String> {
    Ok(auth::hash_password(&password, &unhex(&salt_hex)?))
}

#[spacetimedb::procedure]
pub fn demo_verify_password(
    _ctx: &mut ProcedureContext,
    password: String,
    encoded: String,
) -> Result<bool, String> {
    Ok(auth::verify_password(&password, &encoded))
}

#[spacetimedb::procedure]
pub fn demo_dummy_hash(_ctx: &mut ProcedureContext) -> Result<String, String> {
    Ok(auth::dummy_password_hash())
}

#[spacetimedb::procedure]
pub fn demo_keypair(
    _ctx: &mut ProcedureContext,
    private_hex: String,
) -> Result<DemoKeyInfo, String> {
    let kp = auth::from_private_key_bytes(&unhex(&private_hex)?)?;
    Ok(DemoKeyInfo {
        public_hex: crypto::bytes_to_hex(&kp.public_key),
        private_key_pem: kp.private_key_pem,
        public_key_pem: kp.public_key_pem,
        kid: kp.kid,
    })
}

#[spacetimedb::procedure]
pub fn demo_private_key_from_pem(
    _ctx: &mut ProcedureContext,
    pem: String,
) -> Result<String, String> {
    Ok(crypto::bytes_to_hex(&auth::private_key_from_pem(&pem)?))
}

#[spacetimedb::procedure]
pub fn demo_public_key_from_pem(
    _ctx: &mut ProcedureContext,
    pem: String,
) -> Result<String, String> {
    Ok(crypto::bytes_to_hex(&auth::public_key_from_pem(&pem)?))
}

#[allow(clippy::too_many_arguments)]
#[spacetimedb::procedure]
pub fn demo_session_jwt(
    _ctx: &mut ProcedureContext,
    private_hex: String,
    iss: String,
    sub: String,
    iat: i64,
    exp: i64,
    jti: String,
    kid: Option<String>,
) -> Result<String, String> {
    let claims = auth::session_claims_json(&iss, &sub, iat, exp, &jti);
    auth::sign_jwt(&unhex(&private_hex)?, &claims, kid.as_deref())
}

/// `{"ok":true,"claims":..,"header":..}` or `{"ok":false,"reason":..}`, like TS `verifyJwt`.
#[spacetimedb::procedure]
pub fn demo_verify_jwt(
    _ctx: &mut ProcedureContext,
    public_hex: String,
    token: String,
    issuer: Option<String>,
    audience: Option<String>,
    now_seconds: i64,
) -> Result<String, String> {
    let opts = VerifyJwtOptions {
        issuer: issuer.as_deref(),
        audience: audience.as_deref(),
        clock_tolerance_seconds: None,
        now_seconds,
    };
    Ok(
        match auth::verify_jwt(&unhex(&public_hex)?, &token, &opts) {
            JwtVerify::Ok { claims, header } => {
                serde_json::json!({ "ok": true, "claims": claims, "header": header })
            }
            JwtVerify::Err(reason) => serde_json::json!({ "ok": false, "reason": reason }),
        }
        .to_string(),
    )
}

#[spacetimedb::procedure]
pub fn demo_derive_secret(
    _ctx: &mut ProcedureContext,
    key_hex: String,
    counter: u64,
    micros: i64,
) -> Result<String, String> {
    Ok(crypto::bytes_to_hex(&auth::derive_secret(
        &unhex(&key_hex)?,
        counter,
        micros,
    )))
}

#[spacetimedb::procedure]
pub fn demo_uuid(_ctx: &mut ProcedureContext, ms: u64, rand_hex: String) -> Result<String, String> {
    Ok(auth::uuid_v7(ms, &unhex(&rand_hex)?))
}

#[spacetimedb::procedure]
pub fn demo_pkce(_ctx: &mut ProcedureContext, verifier: String) -> Result<String, String> {
    Ok(auth::pkce_challenge(&verifier))
}

#[spacetimedb::procedure]
pub fn demo_safe_redirect(
    _ctx: &mut ProcedureContext,
    value: String,
) -> Result<Option<String>, String> {
    Ok(auth::safe_redirect_path(&value))
}

/// The parsed query as a JSON object (later keys win, like the TS object).
#[spacetimedb::procedure]
pub fn demo_parse_query(_ctx: &mut ProcedureContext, uri: String) -> Result<String, String> {
    let mut m = serde_json::Map::new();
    for (k, v) in auth::parse_query_string(&uri) {
        m.insert(k, serde_json::Value::String(v));
    }
    Ok(serde_json::Value::Object(m).to_string())
}
