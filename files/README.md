# files: store and serve files

Port of [`@spacetimedb/files`](https://github.com/clockworklabs/SpacetimeDB/pull/5905) at
`37ee2ad627568f95df8229647cff5a482d629c25`. It needs the [crypto](../crypto) port next to it (Rust: `crate::crypto`;
C#: `Crypto.cs` in the same project) for the SHA-256 ETags.

**Schema:** no renames.
- **Tables:** `file` (metadata, unique `owner_path_key`, a btree on `owner_user_id` + `path`) and `file_blob` (bytes),
  both private.
- Like TS, the package registers **no reducers**. The host exposes the operations it wants and derives the owner from
  its authenticated caller. The demo uses the caller's identity, as the upstream example does.
- **Column names:** the SHA-256 column is `sha_256_hex` canonically in both ports (Spacetime's snake_case conversion
  splits letters from digits).

**Same behaviour as TS:**
- **Keys and validation:** owner/path keys with UTF-16 lengths, path/prefix/owner/media-type validation, and the
  per-owner quota (replacing a file doesn't count it twice).
- **Limits:** the 4 MB file limit, 1024-char paths and the 1-200 page size.
- **Listing:** listings in path order with cursor paging, using the same prefix upper bound.
- **Serving:** `serve_file` handles `GET`/`HEAD`, `?id=` (parsed like JS `BigInt`), the strong ETag with
  `If-None-Match` → 304, public vs private cache headers, `nosniff`, `sandbox`, and `attachment` for anything but
  PNG/JPEG/GIF/WebP/AVIF.

```rust
#[spacetimedb::reducer]
pub fn upload_file(ctx: &ReducerContext, path: String, mime_type: String, bytes: Vec<u8>, visibility: String) -> Result<(), String> {
    files::upload_file(ctx, &files::UploadFileArgs { path, mime_type, bytes, visibility }, &ctx.sender().to_hex(), &Default::default())?;
    Ok(())
}

#[handler]
fn file_serve(ctx: &mut HandlerContext, req: Request) -> Response { files::serve_file(ctx, req, |_| false) }
```

```csharp
[Reducer]
public static void UploadFile(ReducerContext ctx, string path, string mimeType, byte[] bytes, string visibility) =>
    FilesUpload(FilesTx.Of(ctx), new(path, mimeType, bytes, visibility), ctx.Sender.ToString());

[HttpHandler]
public static HttpResponse FileServe(HandlerContext ctx, HttpRequest req) => FilesServe(ctx, req, _ => false);
```

| TypeScript | Rust | C# |
|---|---|---|
| `uploadFile(ctx, args, owner, { maxOwnerBytes })` | `upload_file(ctx, &UploadFileArgs, owner, &UploadFileOpts)` | `FilesUpload(FilesTx, FilesUploadArgs, owner, maxOwnerBytes)` |
| `renameFile` / `deleteFile` / `setFileVisibility` | `rename_file` / `delete_file` / `set_file_visibility` | `FilesRename` / `FilesDelete` / `FilesSetVisibility` |
| `listFiles(ctx, { prefix, cursor, limit }, owner)` | `list_files(ctx, prefix, cursor, limit, owner)` | `FilesList(tx, prefix, cursor, limit, owner)` |
| `readFileBytes(ctx, { path }, owner)` | `read_file_bytes(ctx, path, owner)` | `FilesReadBytes(tx, path, owner)` |
| `serveFile(ctx, req, canAccess?)` | `serve_file(ctx, req, \|meta\| bool)` | `FilesServe(ctx, req, meta => bool)` |
| `fileSummary`, `errors`, constants | `FileSummary`, `files::errors`, `FILE_*` | `FileSummary`, `FilesErrors`, `File*` |

**Differences:**
- **Errors:** helpers return `Err(code)` (Rust) or throw an `Exception` (C#) with the `files.*` codes; TS throws a
  `SenderError`.
- **Huge ids:** `serve_file` answers 404 for an id beyond `u64`; TS fails the lookup with a 500.

**Tests:** the upstream cases (SHA-256, query parsing, owner/path keys, path and media-type validation, response
headers) plus every helper and the serve route end to end, in [`tests/files.test.mjs`](../tests/files.test.mjs).
