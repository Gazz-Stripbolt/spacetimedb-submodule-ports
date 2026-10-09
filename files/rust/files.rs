//! # files.rs: store and serve files for Rust modules
//!
//! Unofficial Rust port of `@spacetimedb/files` (SpacetimeDB PR #5905, commit
//! `37ee2ad627568f95df8229647cff5a482d629c25`, package `spacetime-files-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Same tables (`file`, `file_blob`), owner/path keys, validation rules, limits, error codes,
//! listing order and HTTP serving (ETag/304, HEAD, cache and safety headers) as the TS package.
//! Like TS, the package registers no reducers: the host exposes the operations it wants and
//! derives the owner from the authenticated caller.
//!
//! Needs the crypto port next to it (`crate::crypto`) and the `http` crate.
//!
//! ```ignore
//! #[spacetimedb::reducer]
//! pub fn upload_file(ctx: &ReducerContext, path: String, mime_type: String, bytes: Vec<u8>, visibility: String) -> Result<(), String> {
//!     files::upload_file(ctx, &files::UploadFileArgs { path, mime_type, bytes, visibility }, &ctx.sender().to_hex(), &Default::default())?;
//!     Ok(())
//! }
//!
//! #[handler]
//! fn file_serve(ctx: &mut HandlerContext, req: Request) -> Response { files::serve_file(ctx, req, |_| false) }
//! // .get("/files", file_serve).head("/files", file_serve)
//! ```
//!
//! Changes from the TypeScript original:
//! - Helpers return `Result<_, String>` with the `files.*` codes where TS throws a `SenderError`.
//! - `serve_file` treats an id beyond `u64` as not found (TS fails the lookup with a 500).

use crate::crypto;
use spacetimedb::http::{Body, HandlerContext, Request, Response};
use spacetimedb::{ReducerContext, SpacetimeType, Table, Timestamp};
use std::ops::Bound;

pub const FILE_BYTES_MAX: usize = 4_000_000;
/// Default total bytes one owner may store.
pub const FILE_OWNER_BYTES_MAX: u64 = 100_000_000;
pub const FILE_PATH_MAX: usize = 1024;
pub const FILE_MIME_TYPE_MAX: usize = 127;
pub const FILE_LIST_PAGE_MAX: u32 = 200;
pub const FILE_VISIBILITY_OWNER: &str = "owner";
pub const FILE_VISIBILITY_PUBLIC: &str = "public";

/// The `files.*` codes. `TOO_LARGE` is followed by `:<bytes>/<max>`.
pub mod errors {
    pub const INVALID_OWNER: &str = "files.invalid_owner";
    pub const INVALID_PATH: &str = "files.invalid_path";
    pub const INVALID_PREFIX: &str = "files.invalid_prefix";
    pub const INVALID_CURSOR: &str = "files.invalid_cursor";
    pub const INVALID_PAGE_SIZE: &str = "files.invalid_page_size";
    pub const INVALID_MIME_TYPE: &str = "files.invalid_mime_type";
    pub const INVALID_VISIBILITY: &str = "files.invalid_visibility";
    pub const TOO_LARGE: &str = "files.too_large";
    pub const QUOTA_EXCEEDED: &str = "files.quota_exceeded";
    pub const NOT_FOUND: &str = "files.not_found";
    pub const PATH_TAKEN: &str = "files.path_taken";
}

// ---------------------------------------------------------------------------
// Tables and types
// ---------------------------------------------------------------------------

#[spacetimedb::table(accessor = file, index(accessor = owner_path, btree(columns = [owner_user_id, path])))]
#[derive(Clone, Debug)]
pub struct File {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    /// Collision-safe internal owner/path key.
    #[unique]
    pub owner_path_key: String,
    #[index(btree)]
    pub path: String,
    #[index(btree)]
    pub owner_user_id: String,
    pub mime_type: String,
    pub size: u64,
    /// Lowercase hex of SHA-256(bytes); the strong ETag.
    pub sha256_hex: String,
    #[index(btree)]
    pub visibility: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Bytes live apart from metadata, so lookups, HEAD and 304s don't read the blob.
#[spacetimedb::table(accessor = file_blob)]
#[derive(Clone, Debug)]
pub struct FileBlob {
    #[primary_key]
    pub file_id: u64,
    pub bytes: Vec<u8>,
}

/// Safe metadata for host views (no owner, owner/path key or bytes).
#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct FileSummary {
    pub id: u64,
    pub path: String,
    pub mime_type: String,
    pub size: u64,
    pub sha256_hex: String,
    pub visibility: String,
    pub updated_at: Timestamp,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct FileListPage {
    pub files: Vec<FileSummary>,
    pub next_cursor: Option<String>,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct FileBytes {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

impl From<&File> for FileSummary {
    fn from(f: &File) -> Self {
        FileSummary {
            id: f.id,
            path: f.path.clone(),
            mime_type: f.mime_type.clone(),
            size: f.size,
            sha256_hex: f.sha256_hex.clone(),
            visibility: f.visibility.clone(),
            updated_at: f.updated_at,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadFileArgs {
    pub path: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub visibility: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UploadFileOpts {
    /// Total bytes this owner may store. Default `FILE_OWNER_BYTES_MAX`.
    pub max_owner_bytes: Option<u64>,
}

// ---------------------------------------------------------------------------
// Validation (lengths are UTF-16 code units, as in TS)
// ---------------------------------------------------------------------------

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn has_control_character(s: &str) -> bool {
    s.chars().any(|c| (c as u32) <= 0x1f || c as u32 == 0x7f)
}

/// JS `String.prototype.trim`.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| {
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
    })
}

pub fn owner_path_key(owner: &str, path: &str) -> String {
    format!("{}:{owner}{path}", utf16_len(owner))
}

pub fn validate_file_owner(owner: &str) -> Result<&str, String> {
    let len = utf16_len(owner);
    if len == 0 || len > 512 || has_control_character(owner) {
        return Err(errors::INVALID_OWNER.into());
    }
    Ok(owner)
}

pub fn validate_file_path(path: &str) -> Result<&str, String> {
    let len = utf16_len(path);
    if !(2..=FILE_PATH_MAX).contains(&len)
        || !path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains("//")
        || has_control_character(path)
    {
        return Err(errors::INVALID_PATH.into());
    }
    for segment in path[1..].split('/') {
        if segment == "." || segment == ".." || utf16_len(segment) > 255 {
            return Err(errors::INVALID_PATH.into());
        }
    }
    Ok(path)
}

pub fn validate_file_prefix(prefix: &str) -> Result<&str, String> {
    if prefix.is_empty() {
        return Ok(prefix);
    }
    if utf16_len(prefix) > FILE_PATH_MAX
        || !prefix.starts_with('/')
        || prefix.contains('\\')
        || prefix.contains("//")
        || has_control_character(prefix)
    {
        return Err(errors::INVALID_PREFIX.into());
    }
    Ok(prefix)
}

fn mime_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$&^_.+-".contains(c)
}

/// A media type like `image/png`, lowercased. Parameters and control characters are rejected.
pub fn validate_mime_type(mime_type: &str) -> Result<String, String> {
    let value = js_trim(mime_type);
    let ok = match value.split_once('/') {
        Some((a, b)) => {
            !a.is_empty() && !b.is_empty() && a.chars().all(mime_char) && b.chars().all(mime_char)
        }
        None => false,
    };
    if value.is_empty() || utf16_len(value) > FILE_MIME_TYPE_MAX || !ok {
        return Err(errors::INVALID_MIME_TYPE.into());
    }
    Ok(value.to_lowercase())
}

pub fn safe_mime_type(mime_type: &str) -> String {
    validate_mime_type(mime_type).unwrap_or_else(|_| "application/octet-stream".into())
}

fn valid_visibility(v: &str) -> Result<&str, String> {
    if v == FILE_VISIBILITY_OWNER || v == FILE_VISIBILITY_PUBLIC {
        Ok(v)
    } else {
        Err(errors::INVALID_VISIBILITY.into())
    }
}

pub fn file_sha256_hex(bytes: &[u8]) -> String {
    crypto::bytes_to_hex(&crypto::sha256(bytes))
}

/// The smallest string greater than every string starting with `prefix`, by code point.
fn prefix_upper_bound(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let mut next = last as u32 + 1;
        if next == 0xd800 {
            next = 0xe000; // no surrogate code points in Rust strings; the next scalar value
        }
        if let Some(c) = char::from_u32(next) {
            chars.push(c);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Operations (run in the caller's transaction; the host passes the owner)
// ---------------------------------------------------------------------------

fn require_file(ctx: &ReducerContext, owner: &str, path: &str) -> Result<File, String> {
    ctx.db
        .file()
        .owner_path_key()
        .find(owner_path_key(owner, path))
        .ok_or_else(|| errors::NOT_FOUND.into())
}

/// Creates or replaces the owner's file at `path`. Returns the file id.
pub fn upload_file(
    ctx: &ReducerContext,
    args: &UploadFileArgs,
    owner: &str,
    opts: &UploadFileOpts,
) -> Result<u64, String> {
    let owner = validate_file_owner(owner)?;
    let path = validate_file_path(&args.path)?;
    let mime_type = validate_mime_type(&args.mime_type)?;
    let visibility = valid_visibility(&args.visibility)?;
    let size = args.bytes.len() as u64;
    if args.bytes.len() > FILE_BYTES_MAX {
        return Err(format!(
            "{}:{}/{FILE_BYTES_MAX}",
            errors::TOO_LARGE,
            args.bytes.len()
        ));
    }
    let key = owner_path_key(owner, path);
    let mut total = size;
    for row in ctx.db.file().owner_user_id().filter(owner) {
        if row.owner_path_key != key {
            total += row.size;
        }
    }
    if total > opts.max_owner_bytes.unwrap_or(FILE_OWNER_BYTES_MAX) {
        return Err(errors::QUOTA_EXCEEDED.into());
    }
    let sha256_hex = file_sha256_hex(&args.bytes);
    if let Some(existing) = ctx.db.file().owner_path_key().find(&key) {
        let id = existing.id;
        ctx.db.file().id().update(File {
            mime_type,
            size,
            sha256_hex,
            visibility: visibility.into(),
            updated_at: ctx.timestamp,
            ..existing
        });
        ctx.db.file_blob().file_id().update(FileBlob {
            file_id: id,
            bytes: args.bytes.clone(),
        });
        return Ok(id);
    }
    let row = ctx.db.file().insert(File {
        id: 0,
        owner_path_key: key,
        path: path.into(),
        owner_user_id: owner.into(),
        mime_type,
        size,
        sha256_hex,
        visibility: visibility.into(),
        created_at: ctx.timestamp,
        updated_at: ctx.timestamp,
    });
    ctx.db.file_blob().insert(FileBlob {
        file_id: row.id,
        bytes: args.bytes.clone(),
    });
    Ok(row.id)
}

/// Moves the owner's file to a free path.
pub fn rename_file(
    ctx: &ReducerContext,
    old_path: &str,
    new_path: &str,
    owner: &str,
) -> Result<(), String> {
    let owner = validate_file_owner(owner)?;
    let old_path = validate_file_path(old_path)?;
    let new_path = validate_file_path(new_path)?;
    if old_path == new_path {
        return Ok(());
    }
    let row = require_file(ctx, owner, old_path)?;
    let key = owner_path_key(owner, new_path);
    if ctx.db.file().owner_path_key().find(&key).is_some() {
        return Err(errors::PATH_TAKEN.into());
    }
    ctx.db.file().id().update(File {
        owner_path_key: key,
        path: new_path.into(),
        updated_at: ctx.timestamp,
        ..row
    });
    Ok(())
}

/// Deletes the owner's file at `path`, if any.
pub fn delete_file(ctx: &ReducerContext, path: &str, owner: &str) -> Result<(), String> {
    let owner = validate_file_owner(owner)?;
    let path = validate_file_path(path)?;
    if let Some(row) = ctx
        .db
        .file()
        .owner_path_key()
        .find(owner_path_key(owner, path))
    {
        ctx.db.file_blob().file_id().delete(row.id);
        ctx.db.file().id().delete(row.id);
    }
    Ok(())
}

/// The owner's files under `prefix`, ordered by path, without bytes. Pass `next_cursor` back
/// in until it's `None`. `limit` is 1-200 (default 100).
pub fn list_files(
    ctx: &ReducerContext,
    prefix: &str,
    cursor: Option<&str>,
    limit: Option<u32>,
    owner: &str,
) -> Result<FileListPage, String> {
    let owner = validate_file_owner(owner)?;
    let prefix = validate_file_prefix(prefix)?;
    let cursor = cursor.map(validate_file_path).transpose()?;
    if let Some(c) = cursor
        && !c.starts_with(prefix)
    {
        return Err(errors::INVALID_CURSOR.into());
    }
    let limit = limit.unwrap_or(100);
    if !(1..=FILE_LIST_PAGE_MAX).contains(&limit) {
        return Err(errors::INVALID_PAGE_SIZE.into());
    }
    let from: Bound<&str> = match cursor {
        Some(c) => Bound::Excluded(c),
        None if prefix.is_empty() => Bound::Unbounded,
        None => Bound::Included(prefix),
    };
    let upper = prefix_upper_bound(prefix);
    let to: Bound<&str> = match &upper {
        Some(u) => Bound::Excluded(u.as_str()),
        None => Bound::Unbounded,
    };
    let mut files: Vec<FileSummary> = Vec::new();
    for row in ctx.db.file().owner_path().filter((owner, (from, to))) {
        files.push(FileSummary::from(&row));
        if files.len() > limit as usize {
            break;
        }
    }
    let has_more = files.len() > limit as usize;
    if has_more {
        files.pop();
    }
    let next_cursor = if has_more {
        files.last().map(|f| f.path.clone())
    } else {
        None
    };
    Ok(FileListPage { files, next_cursor })
}

/// The owner's bytes. HTTP handlers never see the caller's identity, so private files are
/// read here, from a procedure that knows the sender.
pub fn read_file_bytes(ctx: &ReducerContext, path: &str, owner: &str) -> Result<FileBytes, String> {
    let owner = validate_file_owner(owner)?;
    let path = validate_file_path(path)?;
    let row = require_file(ctx, owner, path)?;
    let blob = ctx
        .db
        .file_blob()
        .file_id()
        .find(row.id)
        .ok_or(errors::NOT_FOUND.to_string())?;
    Ok(FileBytes {
        bytes: blob.bytes,
        mime_type: row.mime_type,
    })
}

pub fn set_file_visibility(
    ctx: &ReducerContext,
    path: &str,
    visibility: &str,
    owner: &str,
) -> Result<(), String> {
    let owner = validate_file_owner(owner)?;
    let path = validate_file_path(path)?;
    let visibility = valid_visibility(visibility)?;
    let row = require_file(ctx, owner, path)?;
    ctx.db.file().id().update(File {
        visibility: visibility.into(),
        updated_at: ctx.timestamp,
        ..row
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP serving
// ---------------------------------------------------------------------------

/// What `serve_file`'s access check sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMetadata {
    pub id: u64,
    pub path: String,
    pub owner_user_id: String,
    pub mime_type: String,
    pub size: u64,
    pub sha256_hex: String,
    pub visibility: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

fn snapshot(f: &File) -> FileMetadata {
    FileMetadata {
        id: f.id,
        path: f.path.clone(),
        owner_user_id: f.owner_user_id.clone(),
        mime_type: safe_mime_type(&f.mime_type),
        size: f.size,
        sha256_hex: f.sha256_hex.clone(),
        visibility: f.visibility.clone(),
        created_at: f.created_at,
        updated_at: f.updated_at,
    }
}

const INLINE_MIME_TYPES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
];

/// The response headers for a file. Everything but common images downloads, so stored HTML or
/// SVG never runs on the serving origin.
pub fn response_headers(
    mime_type: &str,
    size: u64,
    sha256_hex: &str,
    visibility: &str,
) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("content-type", mime_type.to_string()),
        ("content-length", size.to_string()),
        ("etag", format!("\"{sha256_hex}\"")),
        (
            "cache-control",
            if visibility == FILE_VISIBILITY_PUBLIC {
                "public, max-age=300, must-revalidate".into()
            } else {
                "private, max-age=60, must-revalidate".into()
            },
        ),
        ("x-content-type-options", "nosniff".into()),
        ("content-security-policy", "sandbox".into()),
    ];
    if !INLINE_MIME_TYPES.contains(&mime_type) {
        h.push(("content-disposition", "attachment".into()));
    }
    h
}

/// One query parameter: `+` is a space; a malformed escape (in any key checked) gives `None`.
pub fn query_param(uri: &str, name: &str) -> Option<String> {
    let q = uri.find('?')?;
    for part in uri[q + 1..].split('&') {
        if part.is_empty() {
            continue;
        }
        let (raw_key, raw_value) = match part.find('=') {
            Some(eq) => (&part[..eq], &part[eq + 1..]),
            None => (part, ""),
        };
        let key = decode_uri_component(&raw_key.replace('+', " "))?;
        if key != name {
            continue;
        }
        return decode_uri_component(&raw_value.replace('+', " "));
    }
    None
}

/// JS `decodeURIComponent`; `None` on malformed escapes or invalid UTF-8.
fn decode_uri_component(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() || !b[i + 1].is_ascii_hexdigit() || !b[i + 2].is_ascii_hexdigit() {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// JS `BigInt(string)`: trims, then decimal (optionally signed) or 0x/0o/0b. `None` = SyntaxError.
fn js_bigint(s: &str) -> Option<i128> {
    let t = js_trim(s);
    if t.is_empty() {
        return Some(0);
    }
    let lower = t.to_ascii_lowercase();
    let (radix, digits) = if let Some(d) = lower.strip_prefix("0x") {
        (16, d)
    } else if let Some(d) = lower.strip_prefix("0o") {
        (8, d)
    } else if let Some(d) = lower.strip_prefix("0b") {
        (2, d)
    } else {
        let (neg, d) = match t.as_bytes()[0] {
            b'-' => (true, &t[1..]),
            b'+' => (false, &t[1..]),
            _ => (false, t),
        };
        if d.is_empty() || !d.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        // Anything past i128 is far beyond u64 anyway.
        let v: i128 = d.parse().unwrap_or(i128::MAX);
        return Some(if neg { -v } else { v });
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    Some(i128::from_str_radix(digits, radix).unwrap_or(i128::MAX))
}

fn text(status: u16, body: &str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from_bytes(body.as_bytes().to_vec()))
        .expect("valid response")
}

fn with_headers(status: u16, headers: &[(&str, String)], body: Vec<u8>) -> Response {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(*k, v.as_str());
    }
    b.body(Body::from_bytes(body)).expect("valid response")
}

/// Serves `?id=<fileId>` for GET and HEAD. Public files are always served; others only when
/// `can_access` returns true.
pub fn serve_file(
    ctx: &mut HandlerContext,
    req: Request,
    can_access: impl Fn(&FileMetadata) -> bool,
) -> Response {
    let method = req.method().as_str().to_ascii_uppercase();
    if method != "GET" && method != "HEAD" {
        return text(405, "method not allowed");
    }
    let Some(raw_id) = query_param(&req.uri().to_string(), "id").filter(|r| !r.is_empty()) else {
        return text(400, "missing id");
    };
    let id = match js_bigint(&raw_id) {
        Some(id) if id > 0 => id,
        _ => return text(400, "bad id"),
    };
    let Ok(id) = u64::try_from(id) else {
        return text(404, "not found");
    };
    let allowed = |f: &FileMetadata| f.visibility == FILE_VISIBILITY_PUBLIC || can_access(f);
    let if_none_match = req
        .headers()
        .get_all("if-none-match")
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    let metadata = ctx.with_tx(|tx| tx.db.file().id().find(id).map(|r| snapshot(&r)));
    let Some(metadata) = metadata else {
        return text(404, "not found");
    };
    if !allowed(&metadata) {
        return text(403, "forbidden");
    }
    let headers = response_headers(
        &metadata.mime_type,
        metadata.size,
        &metadata.sha256_hex,
        &metadata.visibility,
    );
    let etag = format!("\"{}\"", metadata.sha256_hex);
    if if_none_match == etag {
        return with_headers(304, &[("etag", etag)], Vec::new());
    }
    if method == "HEAD" {
        return with_headers(200, &headers, Vec::new());
    }
    // Load bytes only for a GET that needs a body, and recheck access against the same snapshot.
    let file = ctx.with_tx(|tx| {
        let row = tx.db.file().id().find(id)?;
        let blob = tx.db.file_blob().file_id().find(id)?;
        Some((snapshot(&row), blob.bytes))
    });
    let Some((meta, bytes)) = file else {
        return text(404, "not found");
    };
    if !allowed(&meta) {
        return text(403, "forbidden");
    }
    let etag = format!("\"{}\"", meta.sha256_hex);
    if if_none_match == etag {
        return with_headers(304, &[("etag", etag)], Vec::new());
    }
    with_headers(
        200,
        &response_headers(
            &meta.mime_type,
            meta.size,
            &meta.sha256_hex,
            &meta.visibility,
        ),
        bytes,
    )
}
