// Demo for Files.cs: the upstream example's operations (upload, rename, delete, visibility,
// private reads over the connection, a public HTTP download route and a metadata view), with the
// caller's identity as the owner, plus procedures the test suite uses to drive the helpers.
// The C# twin of files/demo/rust.

#pragma warning disable STDB_UNSTABLE

using SpacetimeDB;

public static partial class Module
{
    static string DemoOwner(Identity id) => id.ToString().ToLowerInvariant().Replace("0x", "");

    static Result<T, string> DemoTry<T>(Func<T> f)
    {
        try
        {
            return Result<T, string>.Ok(f());
        }
        catch (Exception e)
        {
            return Result<T, string>.Err(e.Message);
        }
    }

    [SpacetimeDB.Reducer]
    public static void UploadFile(ReducerContext ctx, string path, string mimeType, byte[] bytes, string visibility) =>
        FilesUpload(FilesTx.Of(ctx), new(path, mimeType.Length == 0 ? "application/octet-stream" : mimeType, bytes, visibility), DemoOwner(ctx.Sender));

    [SpacetimeDB.Reducer]
    public static void RenameFile(ReducerContext ctx, string oldPath, string newPath) => FilesRename(FilesTx.Of(ctx), oldPath, newPath, DemoOwner(ctx.Sender));

    [SpacetimeDB.Reducer]
    public static void DeleteFile(ReducerContext ctx, string path) => FilesDelete(FilesTx.Of(ctx), path, DemoOwner(ctx.Sender));

    [SpacetimeDB.Reducer]
    public static void SetFileVisibility(ReducerContext ctx, string path, string visibility) => FilesSetVisibility(FilesTx.Of(ctx), path, visibility, DemoOwner(ctx.Sender));

    [SpacetimeDB.Procedure]
    public static Result<FileListPage, string> ListFiles(ProcedureContext ctx, string prefix, string? cursor, uint? limit) =>
        DemoTry(() => ctx.WithTx(tx => FilesList(FilesTx.Of(tx), prefix, cursor, limit, DemoOwner(ctx.Sender))));

    /// <summary>Private bytes travel over the authenticated connection; HTTP handlers never see the caller.</summary>
    [SpacetimeDB.Procedure]
    public static Result<FileBytes, string> ReadFileBytes(ProcedureContext ctx, string path) =>
        DemoTry(() => ctx.WithTx(tx => FilesReadBytes(FilesTx.Of(tx), path, DemoOwner(ctx.Sender))));

    /// <summary>The caller's files, metadata only.</summary>
    [SpacetimeDB.View(Accessor = "MyFileSummaries", Public = true)]
    public static List<FileSummary> MyFileSummaries(ViewContext ctx) =>
        ctx.Db.File.OwnerUserId.Filter(DemoOwner(ctx.Sender)).Select(FilesSummary).OrderBy(f => f.Path, StringComparer.Ordinal).ToList();

    /// <summary>Serves public files only; private files go through ReadFileBytes.</summary>
    [SpacetimeDB.HttpHandler]
    public static HttpResponse FileServe(HandlerContext ctx, HttpRequest req) => FilesServe(ctx, req, _ => false);

    /// <summary>Same, with an access check that admits everything (exercises canAccess).</summary>
    [SpacetimeDB.HttpHandler]
    public static HttpResponse FileServeAny(HandlerContext ctx, HttpRequest req) => FilesServe(ctx, req, _ => true);

    [SpacetimeDB.HttpRouter]
    public static Router Routes() => Router.New()
        .Get("/files", Handlers.FileServe)
        .Head("/files", Handlers.FileServe)
        .Post("/files", Handlers.FileServe)
        .Get("/files-any", Handlers.FileServeAny)
        .Head("/files-any", Handlers.FileServeAny);

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// <summary>Upload for any owner, with an optional quota (the TS maxOwnerBytes option).</summary>
    [SpacetimeDB.Procedure]
    public static Result<ulong, string> DemoUploadAs(ProcedureContext ctx, string owner, string path, string mimeType, byte[] bytes, string visibility, ulong? maxOwnerBytes) =>
        DemoTry(() => ctx.WithTx(tx => FilesUpload(FilesTx.Of(tx), new(path, mimeType, bytes, visibility), owner, maxOwnerBytes)));

    /// <summary>Upload size zero bytes (too big to send through the HTTP API's JSON).</summary>
    [SpacetimeDB.Procedure]
    public static Result<ulong, string> DemoUploadZeros(ProcedureContext ctx, string owner, string path, uint size) =>
        DemoTry(() => ctx.WithTx(tx => FilesUpload(FilesTx.Of(tx), new(path, "application/octet-stream", new byte[size], "owner"), owner)));

    [SpacetimeDB.Reducer]
    public static void DemoDeleteAs(ReducerContext ctx, string owner, string path) => FilesDelete(FilesTx.Of(ctx), path, owner);

    [SpacetimeDB.Procedure]
    public static Result<FileListPage, string> DemoListAs(ProcedureContext ctx, string owner, string prefix, string? cursor, uint? limit) =>
        DemoTry(() => ctx.WithTx(tx => FilesList(FilesTx.Of(tx), prefix, cursor, limit, owner)));

    [SpacetimeDB.Procedure]
    public static Result<string?, string> DemoQueryParam(ProcedureContext ctx, string uri, string name) => DemoTry(() => FilesQueryParam(uri, name));

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoOwnerPathKey(ProcedureContext ctx, string owner, string path) => DemoTry(() => FilesOwnerPathKey(owner, path));

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoValidatePath(ProcedureContext ctx, string path) => DemoTry(() => FilesValidatePath(path));

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoValidateMime(ProcedureContext ctx, string mimeType) => DemoTry(() => FilesValidateMimeType(mimeType));

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoHashHex(ProcedureContext ctx, byte[] bytes) => DemoTry(() => FilesSha256Hex(bytes));
}
