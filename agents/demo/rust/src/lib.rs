//! Demo for agents.rs: the upstream example module (a chat agent and its summarizer, threads
//! owned by the signed-in auth user, image attachments stored with files, a per-user token
//! cap, admin status and the caller's views), plus test agents pointed at the test suite's
//! mock providers and procedures the suite uses to check the port against vectors from the
//! TS code.

#[path = "../../../rust/agents.rs"]
pub mod agents;
#[path = "../../../../auth/rust/auth.rs"]
pub mod auth;
#[path = "../../../../crypto/rust/crypto.rs"]
pub mod crypto;
#[path = "../../../../files/rust/files.rs"]
pub mod files;
#[path = "../../../../rate-limit/rust/rate_limit.rs"]
pub mod rate_limit;

use std::sync::OnceLock;

use agents::{
    AgentDefinition, AgentDispatch, AgentSpec, AgentsClient, AgentsConfig, ArgType, ChatError,
    ChatRequest, LoopAttachment, LoopMessage, agent_tool, agents_message__view, agents_thread,
    agents_thread__view, agents_thread_lock, agents_thread_lock__view,
};
use auth::{AuthHttpOptions, auth_connection_binding__view};
use files::{UploadFileArgs, UploadFileOpts, file, file__view, file_blob};
use serde_json::{Map, Value, json};
use spacetimedb::http::{HandlerContext, Request, Response, Router, handler, router};
use spacetimedb::{ProcedureContext, ReducerContext, SpacetimeType, Table, Timestamp, ViewContext};

// ---------------------------------------------------------------------------
// The upstream example: model.ts
// ---------------------------------------------------------------------------

/// Per-user token cap; no row means no cap.
#[spacetimedb::table(accessor = token_limit)]
#[derive(Clone)]
pub struct TokenLimit {
    #[primary_key]
    pub singleton: bool,
    pub tokens_per_window: u32,
    pub window_secs: u32,
    pub updated_at: Timestamp,
}

/// One row per user: tokens spent in the user's current window.
#[spacetimedb::table(accessor = token_usage)]
#[derive(Clone)]
pub struct TokenUsage {
    #[primary_key]
    pub owner: String,
    pub tokens: u64,
    pub window_ends_at: Timestamp,
}

#[spacetimedb::table(accessor = message_attachment)]
#[derive(Clone)]
pub struct MessageAttachment {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub file_id: u64,
    #[index(btree)]
    pub message_id: u64,
    #[index(btree)]
    pub thread_id: u64,
    #[index(btree)]
    pub owner_user_id: String,
    pub ordinal: u32,
    pub filename: Option<String>,
    pub created_at: Timestamp,
}

/// The example's `File` view row (renamed: `File` is the files port's table row).
#[derive(SpacetimeType, Clone)]
pub struct AttachmentFile {
    pub id: u64,
    pub file_id: u64,
    pub path: String,
    pub owner_user_id: String,
    pub mime_type: String,
    pub size: u64,
    pub sha256_hex: String,
    pub visibility: String,
    pub filename: Option<String>,
    pub message_id: Option<u64>,
    pub thread_id: Option<u64>,
    pub ordinal: u32,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Rows the test agent's `remember` tool writes, in the host transaction.
#[spacetimedb::table(accessor = demo_tool_note)]
pub struct DemoToolNote {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub text: String,
    pub created_at: Timestamp,
}

// ---------------------------------------------------------------------------
// Agents and tools
// ---------------------------------------------------------------------------

/// `new Date(ms).toISOString()`.
fn iso_time(ts: Timestamp) -> String {
    let ms = ts.to_micros_since_unix_epoch().div_euclid(1000);
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

fn get_time() -> agents::AgentTool {
    agent_tool(
        "returns the current server time as an ISO-8601 string",
        ArgType::Unit,
        |tx, _| Ok(iso_time(tx.timestamp)),
    )
    .unwrap()
}

fn echo_args(_: &ReducerContext, args: &Value) -> Result<String, String> {
    Ok(agents::js_json(args))
}

/// The vector tools (tools/gen-agents-vectors.mjs defines the same ones in TS).
fn kitchen_tools() -> Vec<(&'static str, agents::AgentTool)> {
    let t = |d: &str, a: ArgType, run: agents::ToolRun| agent_tool(d, a, run).unwrap();
    vec![
        (
            "echo",
            t(
                "Echo the validated arguments.",
                ArgType::object(&[
                    ("msg", ArgType::String),
                    ("n", ArgType::option(ArgType::U64)),
                ]),
                echo_args,
            ),
        ),
        (
            "kinds",
            t(
                "  Every scalar kind.  ",
                ArgType::object(&[
                    ("b", ArgType::Bool),
                    ("s", ArgType::String),
                    ("f", ArgType::F64),
                    ("g", ArgType::F32),
                    ("i8", ArgType::I8),
                    ("u8", ArgType::U8),
                    ("i16", ArgType::I16),
                    ("u16", ArgType::U16),
                    ("i32", ArgType::I32),
                    ("u32", ArgType::U32),
                    ("i64", ArgType::I64),
                    ("u64", ArgType::U64),
                ]),
                echo_args,
            ),
        ),
        (
            "nested",
            t(
                "Nested objects, arrays and options.",
                ArgType::object(&[
                    (
                        "point",
                        ArgType::object(&[("x", ArgType::I32), ("y", ArgType::I32)]),
                    ),
                    ("tags", ArgType::array(ArgType::String)),
                    ("maybe", ArgType::option(ArgType::array(ArgType::U8))),
                ]),
                echo_args,
            ),
        ),
        (
            "shape",
            t(
                "A tagged union.",
                ArgType::object(&[
                    (
                        "shape",
                        ArgType::enumeration(&[
                            ("circle", ArgType::F64),
                            ("square", ArgType::object(&[("side", ArgType::F64)])),
                            ("none", ArgType::Unit),
                        ]),
                    ),
                    ("note", ArgType::option(ArgType::String)),
                ]),
                echo_args,
            ),
        ),
        (
            "nothing",
            t("Takes no arguments.", ArgType::Unit, |_, _| {
                Ok("nothing to see".into())
            }),
        ),
        (
            "fail",
            t(
                "Always throws.",
                ArgType::object(&[("reason", ArgType::String)]),
                |_, a| {
                    Err(format!(
                        "boom: {}",
                        a["reason"].as_str().unwrap_or_default()
                    ))
                },
            ),
        ),
        (
            "big",
            t(
                "Returns n characters.",
                ArgType::object(&[("n", ArgType::U32)]),
                |_, a| Ok("x".repeat(a["n"].as_u64().unwrap_or(0) as usize)),
            ),
        ),
    ]
}

fn remember() -> agents::AgentTool {
    agent_tool(
        "Store a note for later.",
        ArgType::object(&[("text", ArgType::String)]),
        |tx, a| {
            let text = a["text"].as_str().unwrap_or_default().to_string();
            if text == "explode" {
                return Err("refusing to remember that".into());
            }
            let row = tx.db.demo_tool_note().insert(DemoToolNote {
                id: 0,
                text,
                created_at: tx.timestamp,
            });
            Ok(format!("remembered #{}", row.id))
        },
    )
    .unwrap()
}

/// The test suite's mock providers (tests/agents.test.mjs).
const MOCK: &str = "http://127.0.0.1:4320";

fn definitions() -> Vec<(&'static str, AgentDefinition)> {
    let chat = agents::define_agent(AgentSpec {
        default_model: "anthropic/claude-haiku-4.5".into(),
        models: vec![
            "anthropic/claude-haiku-4.5".into(),
            "anthropic/claude-sonnet-4.5".into(),
            "openai/gpt-4o-mini".into(),
        ],
        default_system_prompt: Some(
            "You are a helpful assistant. Use tools when they make the answer better.".into(),
        ),
        default_max_turns: Some(10),
        default_max_history_messages: Some(50),
        default_retries: Some(2),
        summarizer_agent_name: Some("summarizer".into()),
        embeddings_provider: Some("openai".into()),
        embeddings_model: Some("text-embedding-3-small".into()),
        rag_top_k: Some(4),
        tools: vec![("get_time", get_time())],
        ..Default::default()
    });
    let summarizer = agents::define_agent(AgentSpec {
        default_model: "anthropic/claude-haiku-4.5".into(),
        default_system_prompt: Some(
            "You produce concise running summaries of chat conversations. Capture facts, decisions, names, numbers, and ongoing tasks the main assistant must remember. Skip pleasantries. If the user provides an existing summary, EXTEND it with the new content. Do not restart from scratch and do not duplicate prior facts. Reply with the updated summary as plain prose, no preamble."
                .into(),
        ),
        default_max_turns: Some(1),
        default_max_history_messages: Some(100),
        default_max_tokens: Some(600),
        default_retries: Some(2),
        ..Default::default()
    });
    let kitchen = agents::define_agent(AgentSpec {
        default_provider: Some("openai".into()),
        default_model: "gpt-test".into(),
        default_retries: Some(0),
        tools: kitchen_tools(),
        ..Default::default()
    });
    let claude = agents::define_agent(AgentSpec {
        default_provider: Some("anthropic".into()),
        default_model: "claude-test".into(),
        models: vec!["claude-big".into()],
        default_system_prompt: Some("Be terse.".into()),
        default_max_tokens: Some(512),
        default_response_format: Some(json!({ "type": "json_object" })),
        tools: vec![("get_time", get_time()), ("remember", remember())],
        ..Default::default()
    });
    vec![
        ("chat", chat.unwrap()),
        ("summarizer", summarizer.unwrap()),
        ("kitchen", kitchen.unwrap()),
        ("claude", claude.unwrap()),
    ]
}

fn client() -> &'static AgentsClient {
    static CLIENT: OnceLock<AgentsClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        AgentsClient::new(AgentsConfig {
            agents: definitions(),
            before_run: Some(|tx, run| check_rate_limit(tx, &run.owner)),
            on_usage: Some(|tx, u| {
                bump_rate_limit(tx, &u.run.owner, u.prompt_tokens + u.completion_tokens)
            }),
            attachments: Some(attachments_for),
            max_threads_per_owner: Some(5),
            urls: vec![
                ("openrouter", format!("{MOCK}/openrouter/chat/completions")),
                ("openai", format!("{MOCK}/openai/chat/completions")),
                ("anthropic", format!("{MOCK}/anthropic/messages")),
                ("openai.embeddings", format!("{MOCK}/openai/embeddings")),
                (
                    "openrouter.embeddings",
                    format!("{MOCK}/openrouter/embeddings"),
                ),
            ],
        })
        .unwrap()
    })
}

// ---------------------------------------------------------------------------
// The upstream example: index.ts
// ---------------------------------------------------------------------------

/// Threads are owned by the auth user id, so the same user works across devices.
fn require_user_id(ctx: &ReducerContext) -> Result<String, String> {
    auth::get_caller_user_id(ctx).ok_or_else(|| "agent.not_authenticated".into())
}

fn open_usage(tx: &ReducerContext, user_id: &str) -> Option<TokenUsage> {
    tx.db
        .token_usage()
        .owner()
        .find(user_id.to_string())
        .filter(|r| r.window_ends_at > tx.timestamp)
}

fn check_rate_limit(tx: &ReducerContext, user_id: &str) -> Result<(), String> {
    let used = open_usage(tx, user_id).map_or(0, |r| r.tokens);
    match tx.db.token_limit().singleton().find(true) {
        Some(limit) if used >= limit.tokens_per_window as u64 => Err(format!(
            "agent.rate_limited:{used}/{}",
            limit.tokens_per_window
        )),
        _ => Ok(()),
    }
}

fn bump_rate_limit(tx: &ReducerContext, user_id: &str, tokens: u64) {
    let Some(limit) = tx.db.token_limit().singleton().find(true) else {
        return;
    };
    let open = open_usage(tx, user_id);
    let row = TokenUsage {
        owner: user_id.into(),
        tokens: open.as_ref().map_or(0, |o| o.tokens) + tokens,
        window_ends_at: open.map_or_else(
            || {
                Timestamp::from_micros_since_unix_epoch(
                    tx.timestamp.to_micros_since_unix_epoch()
                        + limit.window_secs as i64 * 1_000_000,
                )
            },
            |o| o.window_ends_at,
        ),
    };
    if tx
        .db
        .token_usage()
        .owner()
        .find(user_id.to_string())
        .is_some()
    {
        tx.db.token_usage().owner().update(row);
    } else {
        tx.db.token_usage().insert(row);
    }
}

fn attachments_for(tx: &ReducerContext, message_id: u64) -> Vec<LoopAttachment> {
    let mut rows: Vec<MessageAttachment> = tx
        .db
        .message_attachment()
        .message_id()
        .filter(message_id)
        .collect();
    rows.sort_by_key(|r| r.ordinal);
    rows.iter()
        .filter_map(|row| {
            let file = tx.db.file().id().find(row.file_id)?;
            let blob = tx.db.file_blob().file_id().find(row.file_id)?;
            Some(LoopAttachment {
                mime_type: file.mime_type,
                data: auth::base64_encode(&blob.bytes),
            })
        })
        .collect()
}

const ATTACHMENT_ALLOWED_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];
const ATTACHMENT_COUNT_MAX: usize = 4;
const ATTACHMENT_TOTAL_BYTES_MAX: usize = 12_000_000;

#[derive(SpacetimeType, Clone)]
pub struct SendAttachment {
    pub mime_type: String,
    pub filename: Option<String>,
    pub bytes: Vec<u8>,
}

fn attachment_validation_error(attachments: &[SendAttachment]) -> Option<String> {
    if attachments.len() > ATTACHMENT_COUNT_MAX {
        return Some(format!(
            "agent.too_many_attachments:{}/{ATTACHMENT_COUNT_MAX}",
            attachments.len()
        ));
    }
    let mut total = 0;
    for a in attachments {
        if !ATTACHMENT_ALLOWED_MIMES.contains(&a.mime_type.as_str()) {
            return Some(format!("agent.unsupported_attachment_mime:{}", a.mime_type));
        }
        if a.bytes.len() > files::FILE_BYTES_MAX {
            return Some(format!(
                "agent.attachment_too_large:{}/{}",
                a.bytes.len(),
                files::FILE_BYTES_MAX
            ));
        }
        total += a.bytes.len();
        if total > ATTACHMENT_TOTAL_BYTES_MAX {
            return Some(format!(
                "agent.attachments_too_large:{total}/{ATTACHMENT_TOTAL_BYTES_MAX}"
            ));
        }
    }
    None
}

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    auth::install(ctx);
    agents::install(ctx);
}

/// Admin-gated; both values set enables the cap, both unset removes it.
#[spacetimedb::reducer]
pub fn set_token_limit(
    ctx: &ReducerContext,
    tokens_per_window: Option<u32>,
    window_secs: Option<u32>,
) -> Result<(), String> {
    if !client().is_admin(ctx) {
        return Err("agent.not_authorized".into());
    }
    if tokens_per_window.is_none() && window_secs.is_none() {
        ctx.db.token_limit().singleton().delete(true);
        return Ok(());
    }
    let (Some(tokens_per_window), Some(window_secs)) = (
        tokens_per_window.filter(|t| *t > 0),
        window_secs.filter(|w| *w > 0),
    ) else {
        return Err("agent.invalid_token_limit".into());
    };
    let row = TokenLimit {
        singleton: true,
        tokens_per_window,
        window_secs,
        updated_at: ctx.timestamp,
    };
    if ctx.db.token_limit().singleton().find(true).is_some() {
        ctx.db.token_limit().singleton().update(row);
    } else {
        ctx.db.token_limit().insert(row);
    }
    Ok(())
}

#[derive(SpacetimeType)]
pub struct AgentInfo {
    pub name: String,
    pub model: String,
    pub models: Vec<String>,
}

#[derive(SpacetimeType)]
pub struct AgentConfigStatus {
    pub is_configured: bool,
    pub stale_lock_threshold_secs: u32,
    pub rate_limit_tokens_per_window: Option<u32>,
    pub rate_limit_window_secs: Option<u32>,
    pub agents: Vec<AgentInfo>,
    pub configured_providers: Vec<String>,
}

#[spacetimedb::procedure]
pub fn get_agent_config_status(ctx: &mut ProcedureContext) -> Result<AgentConfigStatus, String> {
    ctx.try_with_tx(|tx| {
        let status = client().status(tx)?;
        let limit = tx.db.token_limit().singleton().find(true);
        Ok(AgentConfigStatus {
            is_configured: !status.configured_providers.is_empty(),
            stale_lock_threshold_secs: status.stale_lock_threshold_secs,
            rate_limit_tokens_per_window: limit.as_ref().map(|l| l.tokens_per_window),
            rate_limit_window_secs: limit.as_ref().map(|l| l.window_secs),
            agents: status
                .agents
                .into_iter()
                .map(|a| AgentInfo {
                    name: a.name,
                    model: a.model,
                    models: a.models,
                })
                .collect(),
            configured_providers: status.configured_providers,
        })
    })
}

#[spacetimedb::procedure]
pub fn start_thread(
    ctx: &mut ProcedureContext,
    agent_name: String,
    title: Option<String>,
    metadata: Option<String>,
) -> Result<u64, String> {
    ctx.try_with_tx(|tx| {
        let owner = require_user_id(tx)?;
        client().start_thread(tx, &owner, &agent_name, title.clone(), metadata.clone())
    })
}

#[allow(clippy::too_many_arguments)]
#[spacetimedb::reducer]
pub fn update_thread(
    ctx: &ReducerContext,
    thread_id: u64,
    title: Option<String>,
    model_override: Option<String>,
    metadata: Option<String>,
    clear_title: bool,
    clear_model_override: bool,
    clear_metadata: bool,
) -> Result<(), String> {
    let owner = require_user_id(ctx)?;
    client().update_thread(
        ctx,
        &owner,
        thread_id,
        title,
        model_override,
        metadata,
        clear_title,
        clear_model_override,
        clear_metadata,
    )
}

#[spacetimedb::reducer]
pub fn delete_thread(ctx: &ReducerContext, thread_id: u64) -> Result<(), String> {
    let owner = require_user_id(ctx)?;
    client().delete_thread(ctx, &owner, thread_id)?;
    for a in ctx
        .db
        .message_attachment()
        .thread_id()
        .filter(thread_id)
        .collect::<Vec<_>>()
    {
        if let Some(file) = ctx.db.file().id().find(a.file_id) {
            files::delete_file(ctx, &file.path, &owner)?;
        }
        ctx.db.message_attachment().id().delete(a.id);
    }
    Ok(())
}

/// No-op if the thread already has a title.
#[spacetimedb::procedure]
pub fn generate_thread_title(ctx: &mut ProcedureContext, thread_id: u64) -> Result<(), String> {
    let owner = ctx.try_with_tx(|tx| require_user_id(tx))?;
    client().generate_thread_title(ctx, &owner, thread_id)
}

#[spacetimedb::reducer]
pub fn request_cancel(ctx: &ReducerContext, thread_id: u64) -> Result<(), String> {
    let owner = require_user_id(ctx)?;
    client().request_cancel(ctx, &owner, thread_id)
}

#[spacetimedb::procedure]
pub fn send_message(
    ctx: &mut ProcedureContext,
    thread_id: u64,
    content: String,
    attachments: Vec<SendAttachment>,
) -> Result<(), String> {
    if content.is_empty() && attachments.is_empty() {
        return Err("agent.empty_message".into());
    }
    if let Some(e) = attachment_validation_error(&attachments) {
        return Err(e);
    }
    let owner = ctx.try_with_tx(|tx| require_user_id(tx))?;
    let on_insert = |tx: &ReducerContext, message_id: u64| -> Result<(), String> {
        for (ordinal, a) in attachments.iter().enumerate() {
            let file_id = files::upload_file(
                tx,
                &UploadFileArgs {
                    path: format!("/msg/{message_id}/{ordinal}"),
                    mime_type: a.mime_type.clone(),
                    bytes: a.bytes.clone(),
                    visibility: files::FILE_VISIBILITY_OWNER.into(),
                },
                &owner,
                &UploadFileOpts::default(),
            )?;
            tx.db.message_attachment().insert(MessageAttachment {
                id: 0,
                file_id,
                message_id,
                thread_id,
                owner_user_id: owner.clone(),
                ordinal: ordinal as u32,
                filename: a.filename.clone(),
                created_at: tx.timestamp,
            });
        }
        Ok(())
    };
    client().send_message(ctx, &owner, thread_id, &content, Some(&on_insert))
}

#[spacetimedb::procedure]
pub fn regenerate_response(ctx: &mut ProcedureContext, thread_id: u64) -> Result<(), String> {
    let owner = ctx.try_with_tx(|tx| require_user_id(tx))?;
    client().regenerate_response(ctx, &owner, thread_id)
}

// ---------------------------------------------------------------------------
// The upstream example: views.ts
// ---------------------------------------------------------------------------

fn caller_user_id(ctx: &ViewContext) -> Option<String> {
    ctx.db
        .auth_connection_binding()
        .stdb_identity()
        .find(ctx.sender())
        .map(|b| b.user_id)
}

#[spacetimedb::view(accessor = my_threads, public)]
fn my_threads(ctx: &ViewContext) -> Vec<agents::AgentsThread> {
    caller_user_id(ctx).map_or(vec![], |u| {
        ctx.db.agents_thread().owner().filter(u.as_str()).collect()
    })
}

#[spacetimedb::view(accessor = my_messages, public)]
fn my_messages(ctx: &ViewContext) -> Vec<agents::AgentsMessage> {
    caller_user_id(ctx).map_or(vec![], |u| {
        ctx.db.agents_message().owner().filter(u.as_str()).collect()
    })
}

#[spacetimedb::view(accessor = my_thread_locks, public)]
fn my_thread_locks(ctx: &ViewContext) -> Vec<agents::AgentsThreadLock> {
    caller_user_id(ctx).map_or(vec![], |u| {
        ctx.db
            .agents_thread_lock()
            .owner()
            .filter(u.as_str())
            .collect()
    })
}

#[spacetimedb::view(accessor = my_files, public)]
fn my_files(ctx: &ViewContext) -> Vec<AttachmentFile> {
    let Some(user_id) = caller_user_id(ctx) else {
        return vec![];
    };
    ctx.db
        .message_attachment()
        .owner_user_id()
        .filter(user_id.as_str())
        .filter_map(|a| {
            let file = ctx.db.file().id().find(a.file_id)?;
            Some(AttachmentFile {
                id: a.id,
                file_id: a.file_id,
                path: file.path,
                owner_user_id: a.owner_user_id,
                mime_type: file.mime_type,
                size: file.size,
                sha256_hex: file.sha256_hex,
                visibility: file.visibility,
                filename: a.filename,
                message_id: Some(a.message_id),
                thread_id: Some(a.thread_id),
                ordinal: a.ordinal,
                created_at: a.created_at,
                updated_at: file.updated_at,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// HTTP routes (auth and owner-only attachment downloads)
// ---------------------------------------------------------------------------

/// Development mailer that logs messages.
fn console_send_mail(_: &mut HandlerContext, mail: &auth::MailParams) {
    log::info!(
        "[mail] to={} subject={}\n{}",
        mail.to,
        mail.subject,
        mail.text
    );
}

/// Local plain-HTTP development: no `Secure` cookies; the tests send `x-real-ip`.
const AUTH_HTTP: AuthHttpOptions = AuthHttpOptions {
    trusted_proxy_header: Some(auth::TrustedProxyHeader::XRealIp),
    secure_cookies: false,
    send_mail: Some(console_send_mail),
    app_name: None,
    email_verified_redirect: None,
};

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

/// Attachments are owner-only; the request's session cookie or bearer token identifies the owner.
#[handler]
fn route_file_serve(ctx: &mut HandlerContext, req: Request) -> Response {
    let user_id = ctx.with_tx(|tx| auth::request_user_id(tx, &req));
    files::serve_file(ctx, req, |f| {
        user_id.as_deref() == Some(f.owner_user_id.as_str())
    })
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
        .post("/auth/password/forgot", route_auth_password_forgot)
        .post("/auth/password/reset", route_auth_password_reset)
        .post(
            "/auth/email/verify-request",
            route_auth_email_verify_request,
        )
        .get("/auth/email/verify", route_auth_email_verify)
        .get("/files", route_file_serve)
        .get("/files/", route_file_serve)
        .head("/files/", route_file_serve)
        .head("/files", route_file_serve)
}

// ---------------------------------------------------------------------------
// Test helpers: vectors from the TS code
// ---------------------------------------------------------------------------

fn kitchen() -> &'static AgentDefinition {
    client().agent_def("kitchen").unwrap()
}

/// The `kitchen` agent's tool definitions, as JSON.
#[spacetimedb::procedure]
pub fn demo_tool_defs(_ctx: &mut ProcedureContext) -> String {
    Value::Array(kitchen().dispatch.llm_tool_defs.clone()).to_string()
}

/// Invoke a `kitchen` tool: `{"result", "isError"}`.
#[spacetimedb::procedure]
pub fn demo_invoke(ctx: &mut ProcedureContext, tool: String, input: String) -> String {
    let r = ctx.with_tx(|tx| kitchen().dispatch.invoke(tx, &tool, &input));
    json!({ "result": r.result, "isError": r.is_error }).to_string()
}

/// The definition-time errors for the cases in the vectors.
#[spacetimedb::procedure]
pub fn demo_definition_errors(_ctx: &mut ProcedureContext) -> String {
    let unit = || agent_tool("ok", ArgType::Unit, |_, _| Ok(String::new())).unwrap();
    let spec = |f: &dyn Fn(&mut AgentSpec)| {
        let mut s = AgentSpec {
            default_model: "m".into(),
            ..Default::default()
        };
        f(&mut s);
        agents::define_agent(s).err().unwrap_or_default()
    };
    let many: Vec<String> = (0..65).map(|i| format!("t{i}")).collect();
    let long_name = "a".repeat(65);
    let cases: Vec<(&str, String)> = vec![
        (
            "u128",
            agents::type_to_json_schema(&ArgType::object(&[("x", ArgType::U128)])).unwrap_err(),
        ),
        (
            "i128",
            agents::type_to_json_schema(&ArgType::object(&[("x", ArgType::option(ArgType::I128))]))
                .unwrap_err(),
        ),
        (
            "top_string",
            agents::type_to_json_schema(&ArgType::String).unwrap_err(),
        ),
        (
            "top_array",
            agents::type_to_json_schema(&ArgType::array(ArgType::String)).unwrap_err(),
        ),
        (
            "bad_tool_name",
            AgentDispatch::new(vec![("has spaces", unit())])
                .err()
                .unwrap(),
        ),
        (
            "long_tool_name",
            AgentDispatch::new(vec![(long_name.as_str(), unit())])
                .err()
                .unwrap(),
        ),
        (
            "too_many_tools",
            AgentDispatch::new(many.iter().map(|n| (n.as_str(), unit())).collect())
                .err()
                .unwrap(),
        ),
        (
            "description_empty",
            agent_tool("   ", ArgType::Unit, |_, _| Ok(String::new()))
                .err()
                .unwrap(),
        ),
        (
            "description_long",
            agent_tool(&"d".repeat(1025), ArgType::Unit, |_, _| Ok(String::new()))
                .err()
                .unwrap(),
        ),
        ("model_empty", spec(&|s| s.default_model = "  ".into())),
        ("model_long", spec(&|s| s.default_model = "m".repeat(257))),
        (
            "provider_long",
            spec(&|s| s.default_provider = Some("p".repeat(65))),
        ),
        (
            "prompt_long",
            spec(&|s| s.default_system_prompt = Some("s".repeat(32769))),
        ),
        ("turns_zero", spec(&|s| s.default_max_turns = Some(0))),
        ("turns_big", spec(&|s| s.default_max_turns = Some(101))),
        (
            "history_big",
            spec(&|s| s.default_max_history_messages = Some(1001)),
        ),
        ("tokens_zero", spec(&|s| s.default_max_tokens = Some(0))),
        ("retries_big", spec(&|s| s.default_retries = Some(11))),
        ("rag_big", spec(&|s| s.rag_top_k = Some(101))),
        ("models_empty", spec(&|s| s.models = vec![" ".into()])),
        (
            "summarizer_empty",
            spec(&|s| s.summarizer_agent_name = Some(String::new())),
        ),
        (
            "embeddings_model_long",
            spec(&|s| s.embeddings_model = Some("e".repeat(257))),
        ),
        (
            "bad_agent_name",
            client_err(vec![("a b", plain_agent(None))], None),
        ),
        (
            "unknown_summarizer",
            client_err(vec![("chat", plain_agent(Some("nobody")))], None),
        ),
        (
            "max_threads_zero",
            client_err(vec![("chat", plain_agent(None))], Some(0)),
        ),
    ];
    Value::Object(
        cases
            .into_iter()
            .map(|(k, v)| (k.to_string(), Value::String(v)))
            .collect::<Map<_, _>>(),
    )
    .to_string()
}

fn plain_agent(summarizer: Option<&str>) -> AgentDefinition {
    agents::define_agent(AgentSpec {
        default_model: "m".into(),
        summarizer_agent_name: summarizer.map(str::to_string),
        ..Default::default()
    })
    .unwrap()
}

fn client_err(agents: Vec<(&'static str, AgentDefinition)>, max_threads: Option<usize>) -> String {
    AgentsClient::new(AgentsConfig {
        agents,
        max_threads_per_owner: max_threads,
        ..Default::default()
    })
    .err()
    .unwrap_or_default()
}

/// `defineAgent` defaults and trimming, in the TS shape.
#[spacetimedb::procedure]
pub fn demo_defaults(_ctx: &mut ProcedureContext) -> String {
    let a = agents::define_agent(AgentSpec {
        default_model: "  m/x  ".into(),
        models: vec![" a ".into(), "b".into()],
        summarizer_agent_name: Some(" s ".into()),
        ..Default::default()
    })
    .unwrap();
    let mut o = Map::new();
    o.insert("defaultProvider".into(), a.default_provider.into());
    o.insert("defaultModel".into(), a.default_model.into());
    o.insert("models".into(), json!(a.models));
    if let Some(p) = a.default_system_prompt {
        o.insert("defaultSystemPrompt".into(), p.into());
    }
    o.insert("defaultMaxTurns".into(), a.default_max_turns.into());
    o.insert(
        "defaultMaxHistoryMessages".into(),
        a.default_max_history_messages.into(),
    );
    if let Some(t) = a.default_max_tokens {
        o.insert("defaultMaxTokens".into(), t.into());
    }
    o.insert("defaultRetries".into(), a.default_retries.into());
    if let Some(s) = a.summarizer_agent_name {
        o.insert("summarizerAgentName".into(), s.into());
    }
    o.insert("ragTopK".into(), a.rag_top_k.into());
    Value::Object(o).to_string()
}

/// `provider.buildRequest(request)`: `{url, headers: [[k, v]], body}`.
#[spacetimedb::procedure]
pub fn demo_build_request(
    _ctx: &mut ProcedureContext,
    provider: String,
    request_json: String,
) -> Result<String, String> {
    let p = agents::built_in_provider(&provider).ok_or("unknown provider")?;
    let req =
        ChatRequest::from_json(&serde_json::from_str(&request_json).map_err(|e| e.to_string())?);
    let b = p.build_request(&req);
    Ok(json!({ "url": b.url, "headers": b.headers.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(), "body": b.body }).to_string())
}

/// `provider.parseResponse(text, 'requested-model')`: `{ok: response}` or `{error: message}`.
#[spacetimedb::procedure]
pub fn demo_parse_response(
    _ctx: &mut ProcedureContext,
    provider: String,
    text: String,
) -> Result<String, String> {
    let p = agents::built_in_provider(&provider).ok_or("unknown provider")?;
    Ok(match p.parse_response(&text, "requested-model") {
        Ok(r) => json!({ "ok": r.to_json() }),
        Err(e) => json!({ "error": e }),
    }
    .to_string())
}

/// Replays canned responses: `[status, body]`, or `["throw", message]` for a transport error.
struct ScriptedHttp {
    responses: Vec<Value>,
    attempts: usize,
    sent: Option<Value>,
}

impl agents::HttpLike for ScriptedHttp {
    fn post(
        &mut self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String> {
        self.sent = Some(
            json!({ "url": url, "headers": headers.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(), "body": body }),
        );
        let r = &self.responses[self.attempts];
        self.attempts += 1;
        let text = r[1].as_str().unwrap_or_default().to_string();
        match r[0].as_u64() {
            Some(status) => Ok((status as u16, text)),
            None => Err(text),
        }
    }
}

fn result_json(r: Result<Option<String>, ChatError>) -> Value {
    match r {
        Ok(text) => json!({ "ok": true, "text": text }),
        Err(e) => json!({ "ok": false, "error": e.to_json() }),
    }
}

/// `callChat` (OpenAI shape) over a scripted transport: `{attempts, result}`.
#[spacetimedb::procedure]
pub fn demo_call_chat(
    _ctx: &mut ProcedureContext,
    retries: u32,
    responses_json: String,
) -> Result<String, String> {
    let responses: Vec<Value> = serde_json::from_str(&responses_json).map_err(|e| e.to_string())?;
    let mut http = ScriptedHttp {
        responses,
        attempts: 0,
        sent: None,
    };
    let p = agents::built_in_provider("openai").unwrap();
    let req = ChatRequest {
        api_key: "k".into(),
        model: "m".into(),
        messages: vec![agents::ChatMessage::Message {
            role: "user".into(),
            content: agents::Content::Text("hi".into()),
            tool_calls: None,
        }],
        retries,
        ..Default::default()
    };
    let r = agents::call_chat(&mut http, &p, &req).map(|r| r.text);
    Ok(json!({ "attempts": http.attempts, "result": result_json(r) }).to_string())
}

/// An embeddings provider over a scripted transport: `{sent, result}` in the TS shapes.
#[spacetimedb::procedure]
pub fn demo_embed(
    _ctx: &mut ProcedureContext,
    provider: String,
    texts_json: String,
    status: u16,
    body: String,
) -> Result<String, String> {
    let texts: Vec<String> = serde_json::from_str(&texts_json).map_err(|e| e.to_string())?;
    let url = agents::default_url(&format!("{provider}.embeddings")).ok_or("unknown provider")?;
    let mut http = ScriptedHttp {
        responses: vec![json!([status, body])],
        attempts: 0,
        sent: None,
    };
    let result = match agents::embed(&mut http, url, "ek", "emb-model", &texts) {
        Ok(r) => json!({
            "ok": true,
            "vectors": r.vectors,
            "model": r.model,
            "usage": { "promptTokens": r.prompt_tokens, "totalTokens": r.total_tokens },
        }),
        Err(e) => json!({ "ok": false, "error": e.to_json() }),
    };
    Ok(json!({ "sent": http.sent, "result": result }).to_string())
}

#[spacetimedb::procedure]
pub fn demo_cosine(
    _ctx: &mut ProcedureContext,
    a_json: String,
    b_json: String,
) -> Result<f64, String> {
    let a: Vec<f64> = serde_json::from_str(&a_json).map_err(|e| e.to_string())?;
    let b: Vec<f64> = serde_json::from_str(&b_json).map_err(|e| e.to_string())?;
    Ok(agents::cosine_similarity(&a, &b))
}

/// Indices of the `k` best scores, best first.
#[spacetimedb::procedure]
pub fn demo_top_k(
    _ctx: &mut ProcedureContext,
    scores_json: String,
    k: u32,
) -> Result<Vec<u32>, String> {
    let scores: Vec<f64> = serde_json::from_str(&scores_json).map_err(|e| e.to_string())?;
    let idx: Vec<u32> = (0..scores.len() as u32).collect();
    Ok(
        agents::top_k_by_score(&idx, |i| scores[*i as usize], k as usize)
            .into_iter()
            .map(|(i, _)| i)
            .collect(),
    )
}

fn loop_messages(json: &str) -> Result<Vec<LoopMessage>, String> {
    let rows: Vec<Value> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|r| LoopMessage {
            id: r["id"].as_u64().unwrap_or(0),
            role: r["role"].as_str().unwrap_or_default().into(),
            content: r["content"].as_str().unwrap_or_default().into(),
            tool_calls_json: r["toolCallsJson"].as_str().map(str::to_string),
            tool_call_id: r["toolCallId"].as_str().map(str::to_string),
            attachments: r["attachments"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|x| LoopAttachment {
                            mime_type: x["mimeType"].as_str().unwrap_or_default().into(),
                            data: x["data"].as_str().unwrap_or_default().into(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect())
}

/// `buildLlmMessages` over the given rows.
#[spacetimedb::procedure]
pub fn demo_history(
    _ctx: &mut ProcedureContext,
    messages_json: String,
    max_history: u32,
) -> Result<String, String> {
    let out = agents::build_llm_messages(&loop_messages(&messages_json)?, max_history);
    Ok(Value::Array(out.iter().map(agents::ChatMessage::to_json).collect()).to_string())
}

/// `pickSummarizationCandidates` + `buildSummarizerUserContent`: `{lastNewId, content}`.
#[spacetimedb::procedure]
pub fn demo_summary(
    _ctx: &mut ProcedureContext,
    messages_json: String,
    max_history: u32,
    summarized_through_id: Option<u64>,
    existing: Option<String>,
) -> Result<String, String> {
    let messages = loop_messages(&messages_json)?;
    Ok(match agents::pick_summarization_candidates(&messages, max_history as usize, summarized_through_id) {
        Some((dropped, last)) => json!({ "lastNewId": last, "content": agents::summarizer_user_content(existing.as_deref(), &dropped) }),
        None => json!({ "lastNewId": null, "content": null }),
    }
    .to_string())
}

/// `buildSummarizerUserContent(null, rows)`.
#[spacetimedb::procedure]
pub fn demo_summarizer_content(
    _ctx: &mut ProcedureContext,
    messages_json: String,
) -> Result<String, String> {
    Ok(agents::summarizer_user_content(
        None,
        &loop_messages(&messages_json)?,
    ))
}

#[spacetimedb::procedure]
pub fn demo_context(
    _ctx: &mut ProcedureContext,
    summary: Option<String>,
    snippets: Vec<String>,
) -> Option<String> {
    agents::context_message(summary.as_deref(), &snippets)
}

// ---------------------------------------------------------------------------
// Test helpers: locks
// ---------------------------------------------------------------------------

/// Takes a thread's lock as if a run were in progress (any thread, for the sweep tests).
#[spacetimedb::reducer]
pub fn demo_lock_thread(ctx: &ReducerContext, thread_id: u64) -> Result<(), String> {
    if !client().is_admin(ctx) {
        return Err("agent.not_authorized".into());
    }
    let owner = ctx
        .db
        .agents_thread()
        .id()
        .find(thread_id)
        .map_or(String::new(), |t| t.owner);
    ctx.db
        .agents_thread_lock()
        .insert(agents::AgentsThreadLock {
            thread_id,
            owner,
            locked_at: ctx.timestamp,
            cancel_requested: false,
        });
    Ok(())
}

/// Runs the stale-lock sweep with a batch size; returns how many locks it deleted.
#[spacetimedb::procedure]
pub fn demo_sweep(ctx: &mut ProcedureContext, max_rows: u32) -> Result<u32, String> {
    ctx.try_with_tx(|tx| {
        if !client().is_admin(tx) {
            return Err("agent.not_authorized".into());
        }
        agents::sweep_stale_locks(tx, max_rows as usize).map(|n| n as u32)
    })
}
