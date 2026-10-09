//! Demo for files.rs: the upstream example's operations (upload, rename, move, delete,
//! visibility, private reads over the connection, a public HTTP download route and a
//! metadata view), with the caller's identity as the owner, plus procedures the test suite
//! uses to drive the helpers directly.

#[path = "../../../../crypto/rust/crypto.rs"]
pub mod crypto;
#[path = "../../../rust/files.rs"]
pub mod files;

use files::{FileBytes, FileListPage, FileSummary, UploadFileArgs, UploadFileOpts, file__view};
use spacetimedb::http::{HandlerContext, Request, Response, Router, handler, router};
use spacetimedb::{ProcedureContext, ReducerContext, ViewContext};

fn owner(ctx: &ReducerContext) -> String {
    ctx.sender().to_hex().to_string()
}

#[spacetimedb::reducer]
pub fn upload_file(
    ctx: &ReducerContext,
    path: String,
    mime_type: String,
    bytes: Vec<u8>,
    visibility: String,
) -> Result<(), String> {
    let mime_type = if mime_type.is_empty() {
        "application/octet-stream".into()
    } else {
        mime_type
    };
    files::upload_file(
        ctx,
        &UploadFileArgs {
            path,
            mime_type,
            bytes,
            visibility,
        },
        &owner(ctx),
        &UploadFileOpts::default(),
    )?;
    Ok(())
}

#[spacetimedb::reducer]
pub fn rename_file(ctx: &ReducerContext, old_path: String, new_path: String) -> Result<(), String> {
    files::rename_file(ctx, &old_path, &new_path, &owner(ctx))
}

#[spacetimedb::reducer]
pub fn delete_file(ctx: &ReducerContext, path: String) -> Result<(), String> {
    files::delete_file(ctx, &path, &owner(ctx))
}

#[spacetimedb::reducer]
pub fn set_file_visibility(
    ctx: &ReducerContext,
    path: String,
    visibility: String,
) -> Result<(), String> {
    files::set_file_visibility(ctx, &path, &visibility, &owner(ctx))
}

#[spacetimedb::procedure]
pub fn list_files(
    ctx: &mut ProcedureContext,
    prefix: String,
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<FileListPage, String> {
    let who = ctx.sender().to_hex().to_string();
    ctx.with_tx(|tx| files::list_files(tx, &prefix, cursor.as_deref(), limit, &who))
}

/// Private bytes travel over the authenticated connection; HTTP handlers never see the caller.
#[spacetimedb::procedure]
pub fn read_file_bytes(ctx: &mut ProcedureContext, path: String) -> Result<FileBytes, String> {
    let who = ctx.sender().to_hex().to_string();
    ctx.with_tx(|tx| files::read_file_bytes(tx, &path, &who))
}

/// The caller's files, metadata only.
#[spacetimedb::view(accessor = my_file_summaries, public)]
fn my_file_summaries(ctx: &ViewContext) -> Vec<FileSummary> {
    let who = ctx.sender().to_hex().to_string();
    let mut out: Vec<FileSummary> = ctx
        .db
        .file()
        .owner_user_id()
        .filter(who.as_str())
        .map(|f| FileSummary::from(&f))
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Serves public files only; private files go through `read_file_bytes`.
#[handler]
fn file_serve(ctx: &mut HandlerContext, req: Request) -> Response {
    files::serve_file(ctx, req, |_| false)
}

/// Same, with an access check that admits everything (exercises `can_access`).
#[handler]
fn file_serve_any(ctx: &mut HandlerContext, req: Request) -> Response {
    files::serve_file(ctx, req, |_| true)
}

#[router]
fn routes() -> Router {
    Router::new()
        .get("/files", file_serve)
        .head("/files", file_serve)
        .post("/files", file_serve)
        .get("/files-any", file_serve_any)
        .head("/files-any", file_serve_any)
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Upload for any owner, with an optional quota (the TS `maxOwnerBytes` option).
#[spacetimedb::procedure]
pub fn demo_upload_as(
    ctx: &mut ProcedureContext,
    owner: String,
    path: String,
    mime_type: String,
    bytes: Vec<u8>,
    visibility: String,
    max_owner_bytes: Option<u64>,
) -> Result<u64, String> {
    ctx.try_with_tx(|tx| {
        files::upload_file(
            tx,
            &UploadFileArgs {
                path: path.clone(),
                mime_type: mime_type.clone(),
                bytes: bytes.clone(),
                visibility: visibility.clone(),
            },
            &owner,
            &UploadFileOpts { max_owner_bytes },
        )
    })
}

/// Upload `size` zero bytes (too big to send through the HTTP API's JSON).
#[spacetimedb::procedure]
pub fn demo_upload_zeros(
    ctx: &mut ProcedureContext,
    owner: String,
    path: String,
    size: u32,
) -> Result<u64, String> {
    ctx.try_with_tx(|tx| {
        let args = UploadFileArgs {
            path: path.clone(),
            mime_type: "application/octet-stream".into(),
            bytes: vec![0; size as usize],
            visibility: "owner".into(),
        };
        files::upload_file(tx, &args, &owner, &UploadFileOpts::default())
    })
}

#[spacetimedb::reducer]
pub fn demo_delete_as(ctx: &ReducerContext, owner: String, path: String) -> Result<(), String> {
    files::delete_file(ctx, &path, &owner)
}

#[spacetimedb::procedure]
pub fn demo_list_as(
    ctx: &mut ProcedureContext,
    owner: String,
    prefix: String,
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<FileListPage, String> {
    ctx.with_tx(|tx| files::list_files(tx, &prefix, cursor.as_deref(), limit, &owner))
}

#[spacetimedb::procedure]
pub fn demo_query_param(
    _ctx: &mut ProcedureContext,
    uri: String,
    name: String,
) -> Result<Option<String>, String> {
    Ok(files::query_param(&uri, &name))
}

#[spacetimedb::procedure]
pub fn demo_owner_path_key(
    _ctx: &mut ProcedureContext,
    owner: String,
    path: String,
) -> Result<String, String> {
    Ok(files::owner_path_key(&owner, &path))
}

#[spacetimedb::procedure]
pub fn demo_validate_path(_ctx: &mut ProcedureContext, path: String) -> Result<String, String> {
    files::validate_file_path(&path).map(str::to_string)
}

#[spacetimedb::procedure]
pub fn demo_validate_mime(
    _ctx: &mut ProcedureContext,
    mime_type: String,
) -> Result<String, String> {
    files::validate_mime_type(&mime_type)
}

#[spacetimedb::procedure]
pub fn demo_hash_hex(_ctx: &mut ProcedureContext, bytes: Vec<u8>) -> Result<String, String> {
    Ok(files::file_sha256_hex(&bytes))
}
