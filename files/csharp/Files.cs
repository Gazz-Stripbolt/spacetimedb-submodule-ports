// Files.cs: store and serve files for C# modules.
//
// Unofficial C# port of @spacetimedb/files (SpacetimeDB PR #5905, commit
// 37ee2ad627568f95df8229647cff5a482d629c25, package spacetime-files-ts). Licensed Apache-2.0,
// like the original. Original design and behaviour by Clockwork Labs; this port is by Tinker
// (Gazz-Stripbolt).
//
// Same tables (file, file_blob), owner/path keys, validation rules, limits, error codes, listing
// order and HTTP serving (ETag/304, HEAD, cache and safety headers) as the TS package. Like TS,
// the package registers no reducers: the host exposes the operations it wants and derives the
// owner from the authenticated caller.
//
// Needs the crypto port next to it: add crypto/csharp/Crypto.cs and this file to the project.
//
//     [Reducer]
//     public static void UploadFile(ReducerContext ctx, string path, string mimeType, byte[] bytes, string visibility) =>
//         FilesUpload(FilesTx.Of(ctx), new(path, mimeType, bytes, visibility), ctx.Sender.ToString());
//
//     [HttpHandler]
//     public static HttpResponse FileServe(HandlerContext ctx, HttpRequest req) => FilesServe(ctx, req, _ => false);
//
// Changes from the TypeScript original:
// - Helpers throw Exceptions carrying the files.* codes (TS throws a SenderError).
// - Helpers take a FilesTx (database and timestamp), from a reducer or a procedure's WithTx.
// - FilesServe treats an id beyond ulong as not found (TS fails the lookup with a 500).

#pragma warning disable STDB_UNSTABLE
#nullable enable

using System.Numerics;
using System.Text;
using SpacetimeDB;

public static class FilesErrors
{
    public const string InvalidOwner = "files.invalid_owner";
    public const string InvalidPath = "files.invalid_path";
    public const string InvalidPrefix = "files.invalid_prefix";
    public const string InvalidCursor = "files.invalid_cursor";
    public const string InvalidPageSize = "files.invalid_page_size";
    public const string InvalidMimeType = "files.invalid_mime_type";
    public const string InvalidVisibility = "files.invalid_visibility";
    public const string TooLarge = "files.too_large";
    public const string QuotaExceeded = "files.quota_exceeded";
    public const string NotFound = "files.not_found";
    public const string PathTaken = "files.path_taken";
}

/// <summary>Safe metadata for host views (no owner, owner/path key or bytes).</summary>
[SpacetimeDB.Type]
public partial struct FileSummary
{
    public ulong Id;
    public string Path;
    public string MimeType;
    public ulong Size;
    public string Sha256Hex;
    public string Visibility;
    public Timestamp UpdatedAt;
}

[SpacetimeDB.Type]
public partial struct FileListPage
{
    public List<FileSummary> Files;
    public string? NextCursor;
}

[SpacetimeDB.Type]
public partial struct FileBytes
{
    public byte[] Bytes;
    public string MimeType;
}

public sealed record FilesUploadArgs(string Path, string MimeType, byte[] Bytes, string Visibility);

/// <summary>A transaction to run the helpers in: from a reducer, or a procedure's WithTx.</summary>
public readonly record struct FilesTx(Local Db, Timestamp Timestamp)
{
    public static FilesTx Of(ReducerContext ctx) => new(ctx.Db, ctx.Timestamp);
    public static FilesTx Of(ProcedureTxContext tx) => new(tx.Db, tx.Timestamp);
}

/// <summary>What FilesServe's access check sees.</summary>
public sealed record FileMetadata(ulong Id, string Path, string OwnerUserId, string MimeType, ulong Size, string Sha256Hex, string Visibility, Timestamp CreatedAt, Timestamp UpdatedAt);

public static partial class Module
{
    public const int FileBytesMax = 4_000_000;
    /// <summary>Default total bytes one owner may store.</summary>
    public const ulong FileOwnerBytesMax = 100_000_000;
    public const int FilePathMax = 1024;
    public const int FileMimeTypeMax = 127;
    public const uint FileListPageMax = 200;
    public const string FileVisibilityOwner = "owner";
    public const string FileVisibilityPublic = "public";

    [SpacetimeDB.Table(Accessor = "File")]
    [SpacetimeDB.Index.BTree(Accessor = "OwnerPath", Columns = new[] { nameof(OwnerUserId), nameof(Path) })]
    public partial struct File
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        /// <summary>Collision-safe internal owner/path key.</summary>
        [SpacetimeDB.Unique]
        public string OwnerPathKey;
        [SpacetimeDB.Index.BTree]
        public string Path;
        [SpacetimeDB.Index.BTree]
        public string OwnerUserId;
        public string MimeType;
        public ulong Size;
        /// <summary>Lowercase hex of SHA-256(bytes); the strong ETag.</summary>
        public string Sha256Hex;
        [SpacetimeDB.Index.BTree]
        public string Visibility;
        public Timestamp CreatedAt;
        public Timestamp UpdatedAt;
    }

    /// <summary>Bytes live apart from metadata, so lookups, HEAD and 304s don't read the blob.</summary>
    [SpacetimeDB.Table(Accessor = "FileBlob")]
    public partial struct FileBlob
    {
        [SpacetimeDB.PrimaryKey]
        public ulong FileId;
        public byte[] Bytes;
    }

    public static FileSummary FilesSummary(File f) => new()
    {
        Id = f.Id,
        Path = f.Path,
        MimeType = f.MimeType,
        Size = f.Size,
        Sha256Hex = f.Sha256Hex,
        Visibility = f.Visibility,
        UpdatedAt = f.UpdatedAt,
    };

    // -----------------------------------------------------------------------
    // Validation (C# strings are UTF-16, like JS, so lengths match TS)
    // -----------------------------------------------------------------------

    static bool FilesHasControl(string s) => s.Any(c => c <= 0x1f || c == 0x7f);

    static string FilesJsTrim(string s)
    {
        static bool Ws(char c) => c is (char)0x09 or (char)0x0a or (char)0x0b or (char)0x0c or (char)0x0d or (char)0x20 or (char)0xa0 or (char)0x1680
            or (>= (char)0x2000 and <= (char)0x200a) or (char)0x2028 or (char)0x2029 or (char)0x202f or (char)0x205f or (char)0x3000 or (char)0xfeff;
        int a = 0, b = s.Length;
        while (a < b && Ws(s[a])) a++;
        while (b > a && Ws(s[b - 1])) b--;
        return s[a..b];
    }

    public static string FilesOwnerPathKey(string owner, string path) => $"{owner.Length}:{owner}{path}";

    public static string FilesValidateOwner(string owner) =>
        owner.Length == 0 || owner.Length > 512 || FilesHasControl(owner) ? throw new Exception(FilesErrors.InvalidOwner) : owner;

    public static string FilesValidatePath(string path)
    {
        if (path.Length < 2 || path.Length > FilePathMax || !path.StartsWith('/') || path.EndsWith('/') || path.Contains('\\') || path.Contains("//") || FilesHasControl(path))
            throw new Exception(FilesErrors.InvalidPath);
        foreach (var segment in path[1..].Split('/'))
        {
            if (segment is "." or ".." || segment.Length > 255) throw new Exception(FilesErrors.InvalidPath);
        }
        return path;
    }

    public static string FilesValidatePrefix(string prefix)
    {
        if (prefix.Length == 0) return prefix;
        if (prefix.Length > FilePathMax || !prefix.StartsWith('/') || prefix.Contains('\\') || prefix.Contains("//") || FilesHasControl(prefix))
            throw new Exception(FilesErrors.InvalidPrefix);
        return prefix;
    }

    static bool FilesMimeChar(char c) => c is (>= 'A' and <= 'Z') or (>= 'a' and <= 'z') or (>= '0' and <= '9') or '!' or '#' or '$' or '&' or '^' or '_' or '.' or '+' or '-';

    /// <summary>A media type like image/png, lowercased. Parameters and control characters are rejected.</summary>
    public static string FilesValidateMimeType(string mimeType)
    {
        var value = FilesJsTrim(mimeType);
        var slash = value.IndexOf('/');
        var ok = slash > 0 && slash < value.Length - 1 && value[..slash].All(FilesMimeChar) && value[(slash + 1)..].All(FilesMimeChar);
        if (value.Length == 0 || value.Length > FileMimeTypeMax || !ok) throw new Exception(FilesErrors.InvalidMimeType);
        return value.ToLowerInvariant();
    }

    public static string FilesSafeMimeType(string mimeType)
    {
        try
        {
            return FilesValidateMimeType(mimeType);
        }
        catch (Exception)
        {
            return "application/octet-stream";
        }
    }

    static string FilesValidVisibility(string v) =>
        v is FileVisibilityOwner or FileVisibilityPublic ? v : throw new Exception(FilesErrors.InvalidVisibility);

    public static string FilesSha256Hex(byte[] bytes) => Crypto.BytesToHex(Crypto.Sha256(bytes));

    /// <summary>The smallest string greater than every string starting with prefix, by code point.</summary>
    static string? FilesPrefixUpperBound(string prefix)
    {
        var cps = new List<int>();
        for (var i = 0; i < prefix.Length; i += char.IsSurrogatePair(prefix, i) ? 2 : 1) cps.Add(char.ConvertToUtf32(prefix, i));
        while (cps.Count > 0)
        {
            var last = cps[^1];
            cps.RemoveAt(cps.Count - 1);
            var next = last + 1;
            if (next == 0xd800) next = 0xe000; // the next scalar value (no lone surrogates in keys)
            if (next <= 0x10ffff)
            {
                cps.Add(next);
                return string.Concat(cps.Select(char.ConvertFromUtf32));
            }
        }
        return null;
    }

    // -----------------------------------------------------------------------
    // Operations (run in the caller's transaction; the host passes the owner)
    // -----------------------------------------------------------------------

    static File FilesRequire(FilesTx tx, string owner, string path) =>
        tx.Db.File.OwnerPathKey.Find(FilesOwnerPathKey(owner, path)) ?? throw new Exception(FilesErrors.NotFound);

    /// <summary>Creates or replaces the owner's file at Path. Returns the file id.</summary>
    public static ulong FilesUpload(FilesTx tx, FilesUploadArgs args, string owner, ulong? maxOwnerBytes = null)
    {
        owner = FilesValidateOwner(owner);
        var path = FilesValidatePath(args.Path);
        var mimeType = FilesValidateMimeType(args.MimeType);
        var visibility = FilesValidVisibility(args.Visibility);
        var size = (ulong)args.Bytes.Length;
        if (args.Bytes.Length > FileBytesMax) throw new Exception($"{FilesErrors.TooLarge}:{args.Bytes.Length}/{FileBytesMax}");
        var key = FilesOwnerPathKey(owner, path);
        var total = size;
        foreach (var row in tx.Db.File.OwnerUserId.Filter(owner))
        {
            if (row.OwnerPathKey != key) total += row.Size;
        }
        if (total > (maxOwnerBytes ?? FileOwnerBytesMax)) throw new Exception(FilesErrors.QuotaExceeded);
        var sha = FilesSha256Hex(args.Bytes);
        if (tx.Db.File.OwnerPathKey.Find(key) is { } existing)
        {
            tx.Db.File.Id.Update(existing with { MimeType = mimeType, Size = size, Sha256Hex = sha, Visibility = visibility, UpdatedAt = tx.Timestamp });
            tx.Db.FileBlob.FileId.Update(new FileBlob { FileId = existing.Id, Bytes = args.Bytes });
            return existing.Id;
        }
        var inserted = tx.Db.File.Insert(new File { Id = 0, OwnerPathKey = key, Path = path, OwnerUserId = owner, MimeType = mimeType, Size = size, Sha256Hex = sha, Visibility = visibility, CreatedAt = tx.Timestamp, UpdatedAt = tx.Timestamp });
        tx.Db.FileBlob.Insert(new FileBlob { FileId = inserted.Id, Bytes = args.Bytes });
        return inserted.Id;
    }

    /// <summary>Moves the owner's file to a free path.</summary>
    public static void FilesRename(FilesTx tx, string oldPath, string newPath, string owner)
    {
        owner = FilesValidateOwner(owner);
        oldPath = FilesValidatePath(oldPath);
        newPath = FilesValidatePath(newPath);
        if (oldPath == newPath) return;
        var row = FilesRequire(tx, owner, oldPath);
        var key = FilesOwnerPathKey(owner, newPath);
        if (tx.Db.File.OwnerPathKey.Find(key) is not null) throw new Exception(FilesErrors.PathTaken);
        tx.Db.File.Id.Update(row with { OwnerPathKey = key, Path = newPath, UpdatedAt = tx.Timestamp });
    }

    /// <summary>Deletes the owner's file at path, if any.</summary>
    public static void FilesDelete(FilesTx tx, string path, string owner)
    {
        owner = FilesValidateOwner(owner);
        path = FilesValidatePath(path);
        if (tx.Db.File.OwnerPathKey.Find(FilesOwnerPathKey(owner, path)) is not { } row) return;
        tx.Db.FileBlob.FileId.Delete(row.Id);
        tx.Db.File.Id.Delete(row.Id);
    }

    /// <summary>The owner's files under prefix, ordered by path, without bytes. limit is 1-200 (default 100).</summary>
    public static FileListPage FilesList(FilesTx tx, string prefix, string? cursor, uint? limit, string owner)
    {
        owner = FilesValidateOwner(owner);
        prefix = FilesValidatePrefix(prefix);
        if (cursor is not null)
        {
            cursor = FilesValidatePath(cursor);
            if (!cursor.StartsWith(prefix, StringComparison.Ordinal)) throw new Exception(FilesErrors.InvalidCursor);
        }
        var lim = limit ?? 100;
        if (lim < 1 || lim > FileListPageMax) throw new Exception(FilesErrors.InvalidPageSize);
        var upper = FilesPrefixUpperBound(prefix);
        // Walk this owner's paths in index order and apply the bounds (C# index bounds are inclusive).
        var files = new List<FileSummary>();
        var rows = tx.Db.File.OwnerPath.Filter((owner, new Bound<string>(cursor ?? prefix, upper ?? "\U0010FFFF\U0010FFFF")));
        foreach (var row in rows)
        {
            if (cursor is not null && string.CompareOrdinal(row.Path, cursor) == 0) continue;
            if (upper is not null && FilesCompareCodePoints(row.Path, upper) >= 0) break;
            files.Add(FilesSummary(row));
            if (files.Count > lim) break;
        }
        var hasMore = files.Count > lim;
        if (hasMore) files.RemoveAt(files.Count - 1);
        return new FileListPage { Files = files, NextCursor = hasMore ? files[^1].Path : null };
    }

    /// <summary>Code-point order (the order SpacetimeDB's string index uses).</summary>
    static int FilesCompareCodePoints(string a, string b) =>
        Encoding.UTF8.GetBytes(a).AsSpan().SequenceCompareTo(Encoding.UTF8.GetBytes(b));

    /// <summary>The owner's bytes. HTTP handlers never see the caller, so private files are read from a procedure.</summary>
    public static FileBytes FilesReadBytes(FilesTx tx, string path, string owner)
    {
        owner = FilesValidateOwner(owner);
        path = FilesValidatePath(path);
        var row = FilesRequire(tx, owner, path);
        var blob = tx.Db.FileBlob.FileId.Find(row.Id) ?? throw new Exception(FilesErrors.NotFound);
        return new FileBytes { Bytes = blob.Bytes, MimeType = row.MimeType };
    }

    public static void FilesSetVisibility(FilesTx tx, string path, string visibility, string owner)
    {
        owner = FilesValidateOwner(owner);
        path = FilesValidatePath(path);
        visibility = FilesValidVisibility(visibility);
        var row = FilesRequire(tx, owner, path);
        tx.Db.File.Id.Update(row with { Visibility = visibility, UpdatedAt = tx.Timestamp });
    }

    // -----------------------------------------------------------------------
    // HTTP serving
    // -----------------------------------------------------------------------

    static readonly string[] FilesInlineMimeTypes = ["image/png", "image/jpeg", "image/gif", "image/webp", "image/avif"];

    /// <summary>The response headers for a file. Everything but common images downloads.</summary>
    public static List<(string Name, string Value)> FilesResponseHeaders(string mimeType, ulong size, string sha256Hex, string visibility)
    {
        var h = new List<(string, string)>
        {
            ("content-type", mimeType),
            ("content-length", size.ToString()),
            ("etag", $"\"{sha256Hex}\""),
            ("cache-control", visibility == FileVisibilityPublic ? "public, max-age=300, must-revalidate" : "private, max-age=60, must-revalidate"),
            ("x-content-type-options", "nosniff"),
            ("content-security-policy", "sandbox"),
        };
        if (!FilesInlineMimeTypes.Contains(mimeType)) h.Add(("content-disposition", "attachment"));
        return h;
    }

    static string? FilesDecodeUriComponent(string s)
    {
        var bytes = Encoding.UTF8.GetBytes(s);
        var o = new List<byte>(bytes.Length);
        for (var i = 0; i < bytes.Length;)
        {
            if (bytes[i] == (byte)'%')
            {
                if (i + 2 >= bytes.Length || !Uri.IsHexDigit((char)bytes[i + 1]) || !Uri.IsHexDigit((char)bytes[i + 2])) return null;
                o.Add(Convert.ToByte(Encoding.ASCII.GetString(bytes, i + 1, 2), 16));
                i += 3;
            }
            else o.Add(bytes[i++]);
        }
        try
        {
            return new UTF8Encoding(false, true).GetString(o.ToArray());
        }
        catch (ArgumentException)
        {
            return null;
        }
    }

    /// <summary>One query parameter: '+' is a space; a malformed escape in a key checked before the match gives null.</summary>
    public static string? FilesQueryParam(string uri, string name)
    {
        var q = uri.IndexOf('?');
        if (q < 0) return null;
        foreach (var part in uri[(q + 1)..].Split('&'))
        {
            if (part.Length == 0) continue;
            var eq = part.IndexOf('=');
            var key = FilesDecodeUriComponent((eq < 0 ? part : part[..eq]).Replace('+', ' '));
            if (key is null) return null;
            if (key != name) continue;
            return FilesDecodeUriComponent((eq < 0 ? "" : part[(eq + 1)..]).Replace('+', ' '));
        }
        return null;
    }

    /// <summary>JS BigInt(string): trims, then decimal (optionally signed) or 0x/0o/0b. null = SyntaxError.</summary>
    static BigInteger? FilesJsBigInt(string s)
    {
        var t = FilesJsTrim(s);
        if (t.Length == 0) return BigInteger.Zero;
        var lower = t.ToLowerInvariant();
        int radix;
        string digits;
        if (lower.StartsWith("0x")) (radix, digits) = (16, lower[2..]);
        else if (lower.StartsWith("0o")) (radix, digits) = (8, lower[2..]);
        else if (lower.StartsWith("0b")) (radix, digits) = (2, lower[2..]);
        else
        {
            var neg = t[0] == '-';
            var d = t[0] is '-' or '+' ? t[1..] : t;
            if (d.Length == 0 || !d.All(c => c is >= '0' and <= '9')) return null;
            var v = BigInteger.Parse(d);
            return neg ? -v : v;
        }
        if (digits.Length == 0) return null;
        var acc = BigInteger.Zero;
        foreach (var c in digits)
        {
            var dv = c is >= '0' and <= '9' ? c - '0' : c is >= 'a' and <= 'f' ? c - 'a' + 10 : 99;
            if (dv >= radix) return null;
            acc = acc * radix + dv;
        }
        return acc;
    }

    static HttpResponse FilesText(ushort status, string body) =>
        new(status, HttpVersion.Http11, [], HttpBody.FromString(body));

    static HttpResponse FilesWithHeaders(ushort status, IEnumerable<(string Name, string Value)> headers, byte[] body) =>
        new(status, HttpVersion.Http11, headers.Select(h => new HttpHeader(h.Name, h.Value)).ToList(), new HttpBody(body));

    static FileMetadata FilesSnapshot(File f) =>
        new(f.Id, f.Path, f.OwnerUserId, FilesSafeMimeType(f.MimeType), f.Size, f.Sha256Hex, f.Visibility, f.CreatedAt, f.UpdatedAt);

    /// <summary>Serves ?id=&lt;fileId&gt; for GET and HEAD. Public files always; others only when canAccess returns true.</summary>
    public static HttpResponse FilesServe(HandlerContext ctx, HttpRequest req, Func<FileMetadata, bool>? canAccess = null)
    {
        var method = req.Method.Value.ToUpperInvariant();
        if (method != "GET" && method != "HEAD") return FilesText(405, "method not allowed");
        var rawId = FilesQueryParam(req.Uri, "id");
        if (string.IsNullOrEmpty(rawId)) return FilesText(400, "missing id");
        if (FilesJsBigInt(rawId) is not { } big || big <= 0) return FilesText(400, "bad id");
        if (big > ulong.MaxValue) return FilesText(404, "not found");
        var id = (ulong)big;
        bool Allowed(FileMetadata f) => f.Visibility == FileVisibilityPublic || (canAccess?.Invoke(f) ?? false);
        var inm = string.Join(", ", req.Headers.Where(h => string.Equals(h.Name, "if-none-match", StringComparison.OrdinalIgnoreCase)).Select(h => Encoding.UTF8.GetString(h.Value)));
        var metadata = ctx.WithTx(tx => tx.Db.File.Id.Find(id) is { } r ? FilesSnapshot(r) : null);
        if (metadata is null) return FilesText(404, "not found");
        if (!Allowed(metadata)) return FilesText(403, "forbidden");
        var etag = $"\"{metadata.Sha256Hex}\"";
        if (inm == etag) return FilesWithHeaders(304, [("etag", etag)], []);
        if (method == "HEAD") return FilesWithHeaders(200, FilesResponseHeaders(metadata.MimeType, metadata.Size, metadata.Sha256Hex, metadata.Visibility), []);
        // Load bytes only for a GET that needs a body, and recheck access against the same snapshot.
        var file = ctx.WithTx(tx => tx.Db.File.Id.Find(id) is { } r && tx.Db.FileBlob.FileId.Find(id) is { } b ? (FilesSnapshot(r), b.Bytes) : ((FileMetadata, byte[])?)null);
        if (file is not { } f) return FilesText(404, "not found");
        if (!Allowed(f.Item1)) return FilesText(403, "forbidden");
        etag = $"\"{f.Item1.Sha256Hex}\"";
        if (inm == etag) return FilesWithHeaders(304, [("etag", etag)], []);
        return FilesWithHeaders(200, FilesResponseHeaders(f.Item1.MimeType, f.Item1.Size, f.Item1.Sha256Hex, f.Item1.Visibility), f.Item2);
    }
}
