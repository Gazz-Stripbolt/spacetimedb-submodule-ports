// Demo for Agents.cs: the upstream example module (a chat agent and its summarizer, threads owned
// by the signed-in auth user, image attachments stored with files, a per-user token cap, admin
// status and the caller's views), plus test agents pointed at the test suite's mock providers and
// procedures the suite uses to check the port against vectors from the TS code. The C# twin of
// agents/demo/rust.

#pragma warning disable STDB_UNSTABLE
#nullable enable

using SpacetimeDB;

[SpacetimeDB.Type]
public partial struct AttachmentFile
{
    public ulong Id;
    public ulong FileId;
    public string Path;
    public string OwnerUserId;
    public string MimeType;
    public ulong Size;
    public string Sha256Hex;
    public string Visibility;
    public string? Filename;
    public ulong? MessageId;
    public ulong? ThreadId;
    public uint Ordinal;
    public Timestamp CreatedAt;
    public Timestamp UpdatedAt;
}

[SpacetimeDB.Type]
public partial struct SendAttachment
{
    public string MimeType;
    public string? Filename;
    public byte[] Bytes;
}

[SpacetimeDB.Type]
public partial struct AgentInfo
{
    public string Name;
    public string Model;
    public List<string> Models;
}

[SpacetimeDB.Type]
public partial struct AgentConfigStatus
{
    public bool IsConfigured;
    public uint StaleLockThresholdSecs;
    public uint? RateLimitTokensPerWindow;
    public uint? RateLimitWindowSecs;
    public List<AgentInfo> Agents;
    public List<string> ConfiguredProviders;
}

public static partial class Module
{
    // -----------------------------------------------------------------------
    // The upstream example: model.ts
    // -----------------------------------------------------------------------

    /// <summary>Per-user token cap; no row means no cap.</summary>
    [SpacetimeDB.Table(Accessor = "TokenLimit")]
    public partial struct TokenLimit
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public uint TokensPerWindow;
        public uint WindowSecs;
        public Timestamp UpdatedAt;
    }

    /// <summary>One row per user: tokens spent in the user's current window.</summary>
    [SpacetimeDB.Table(Accessor = "TokenUsage")]
    public partial struct TokenUsage
    {
        [SpacetimeDB.PrimaryKey]
        public string Owner;
        public ulong Tokens;
        public Timestamp WindowEndsAt;
    }

    [SpacetimeDB.Table(Accessor = "MessageAttachment")]
    public partial struct MessageAttachment
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        [SpacetimeDB.Index.BTree]
        public ulong FileId;
        [SpacetimeDB.Index.BTree]
        public ulong MessageId;
        [SpacetimeDB.Index.BTree]
        public ulong ThreadId;
        [SpacetimeDB.Index.BTree]
        public string OwnerUserId;
        public uint Ordinal;
        public string? Filename;
        public Timestamp CreatedAt;
    }

    /// <summary>Rows the test agent's remember tool writes, in the host transaction.</summary>
    [SpacetimeDB.Table(Accessor = "DemoToolNote")]
    public partial struct DemoToolNote
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        public string Text;
        public Timestamp CreatedAt;
    }

    // -----------------------------------------------------------------------
    // Agents and tools
    // -----------------------------------------------------------------------

    /// <summary>new Date(ms).toISOString()</summary>
    static string DemoIsoTime(Timestamp ts) =>
        DateTimeOffset.FromUnixTimeMilliseconds(Math.DivRem(ts.MicrosecondsSinceUnixEpoch, 1000, out var r) - (r < 0 ? 1 : 0))
            .UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss.fff'Z'", System.Globalization.CultureInfo.InvariantCulture);

    static AgentTool DemoGetTime => AgentTool.Create("returns the current server time as an ISO-8601 string", AgentArg.Unit, (tx, _) => DemoIsoTime(tx.Timestamp));

    static string DemoEcho(AgentsTx _, object? args) => AgentsJs.Stringify(args);

    /// <summary>The vector tools (tools/gen-agents-vectors.mjs defines the same ones in TS).</summary>
    static List<(string, AgentTool)> DemoKitchenTools =>
    [
        ("echo", AgentTool.Create("Echo the validated arguments.", AgentArg.Object(("msg", AgentArg.String), ("n", AgentArg.Option(AgentArg.U64))), DemoEcho)),
        ("kinds", AgentTool.Create("  Every scalar kind.  ", AgentArg.Object(
            ("b", AgentArg.Bool), ("s", AgentArg.String), ("f", AgentArg.F64), ("g", AgentArg.F32),
            ("i8", AgentArg.I8), ("u8", AgentArg.U8), ("i16", AgentArg.I16), ("u16", AgentArg.U16),
            ("i32", AgentArg.I32), ("u32", AgentArg.U32), ("i64", AgentArg.I64), ("u64", AgentArg.U64)), DemoEcho)),
        ("nested", AgentTool.Create("Nested objects, arrays and options.", AgentArg.Object(
            ("point", AgentArg.Object(("x", AgentArg.I32), ("y", AgentArg.I32))),
            ("tags", AgentArg.Array(AgentArg.String)),
            ("maybe", AgentArg.Option(AgentArg.Array(AgentArg.U8)))), DemoEcho)),
        ("shape", AgentTool.Create("A tagged union.", AgentArg.Object(
            ("shape", AgentArg.Enum(("circle", AgentArg.F64), ("square", AgentArg.Object(("side", AgentArg.F64))), ("none", AgentArg.Unit))),
            ("note", AgentArg.Option(AgentArg.String))), DemoEcho)),
        ("nothing", AgentTool.Create("Takes no arguments.", AgentArg.Unit, (_, _) => "nothing to see")),
        ("fail", AgentTool.Create("Always throws.", AgentArg.Object(("reason", AgentArg.String)), (_, a) => throw new Exception($"boom: {AgentsJs.Get(a, "reason")}"))),
        ("big", AgentTool.Create("Returns n characters.", AgentArg.Object(("n", AgentArg.U32)), (_, a) => new string('x', (int)(double)AgentsJs.Get(a, "n")!))),
    ];

    static AgentTool DemoRemember => AgentTool.Create("Store a note for later.", AgentArg.Object(("text", AgentArg.String)), (tx, a) =>
    {
        var text = (string)AgentsJs.Get(a, "text")!;
        if (text == "explode") throw new Exception("refusing to remember that");
        var row = tx.Db.DemoToolNote.Insert(new DemoToolNote { Id = 0, Text = text, CreatedAt = tx.Timestamp });
        return $"remembered #{row.Id}";
    });

    /// <summary>The test suite's mock providers (tests/agents.test.mjs).</summary>
    const string DemoMock = "http://127.0.0.1:4320";

    static List<(string, AgentDefinition)> DemoDefinitions =>
    [
        ("chat", AgentDefinition.Define(new AgentSpec
        {
            DefaultModel = "anthropic/claude-haiku-4.5",
            Models = ["anthropic/claude-haiku-4.5", "anthropic/claude-sonnet-4.5", "openai/gpt-4o-mini"],
            DefaultSystemPrompt = "You are a helpful assistant. Use tools when they make the answer better.",
            DefaultMaxTurns = 10,
            DefaultMaxHistoryMessages = 50,
            DefaultRetries = 2,
            SummarizerAgentName = "summarizer",
            EmbeddingsProvider = "openai",
            EmbeddingsModel = "text-embedding-3-small",
            RagTopK = 4,
            Tools = [("get_time", DemoGetTime)],
        })),
        ("summarizer", AgentDefinition.Define(new AgentSpec
        {
            DefaultModel = "anthropic/claude-haiku-4.5",
            DefaultSystemPrompt = "You produce concise running summaries of chat conversations. Capture facts, decisions, names, numbers, and ongoing tasks the main assistant must remember. Skip pleasantries. If the user provides an existing summary, EXTEND it with the new content. Do not restart from scratch and do not duplicate prior facts. Reply with the updated summary as plain prose, no preamble.",
            DefaultMaxTurns = 1,
            DefaultMaxHistoryMessages = 100,
            DefaultMaxTokens = 600,
            DefaultRetries = 2,
        })),
        ("kitchen", AgentDefinition.Define(new AgentSpec
        {
            DefaultProvider = "openai",
            DefaultModel = "gpt-test",
            DefaultRetries = 0,
            Tools = DemoKitchenTools,
        })),
        ("claude", AgentDefinition.Define(new AgentSpec
        {
            DefaultProvider = "anthropic",
            DefaultModel = "claude-test",
            Models = ["claude-big"],
            DefaultSystemPrompt = "Be terse.",
            DefaultMaxTokens = 512,
            DefaultResponseFormat = new JsObject { ["type"] = "json_object" },
            Tools = [("get_time", DemoGetTime), ("remember", DemoRemember)],
        })),
    ];

    static AgentsClient? demoClient;

    static AgentsClient DemoClient => demoClient ??= new AgentsClient(new AgentsConfig
    {
        Agents = DemoDefinitions,
        BeforeRun = (tx, run) => DemoCheckRateLimit(tx, run.Owner),
        OnUsage = (tx, u) => DemoBumpRateLimit(tx, u.Run.Owner, (ulong)(u.PromptTokens + u.CompletionTokens)),
        Attachments = DemoAttachmentsFor,
        MaxThreadsPerOwner = 5,
        Urls =
        [
            ("openrouter", $"{DemoMock}/openrouter/chat/completions"),
            ("openai", $"{DemoMock}/openai/chat/completions"),
            ("anthropic", $"{DemoMock}/anthropic/messages"),
            ("openai.embeddings", $"{DemoMock}/openai/embeddings"),
            ("openrouter.embeddings", $"{DemoMock}/openrouter/embeddings"),
        ],
    });

    // -----------------------------------------------------------------------
    // The upstream example: index.ts
    // -----------------------------------------------------------------------

    /// <summary>Threads are owned by the auth user id, so the same user works across devices.</summary>
    static string DemoRequireUserId(AgentsTx tx) =>
        AuthFindCallerUser(new AuthTx(tx.Db, tx.Timestamp, tx.Sender))?.UserId ?? throw new Exception("agent.not_authenticated");

    static TokenUsage? DemoOpenUsage(AgentsTx tx, string userId) =>
        tx.Db.TokenUsage.Owner.Find(userId) is { } r && r.WindowEndsAt.MicrosecondsSinceUnixEpoch > tx.Timestamp.MicrosecondsSinceUnixEpoch ? r : null;

    static void DemoCheckRateLimit(AgentsTx tx, string userId)
    {
        var used = DemoOpenUsage(tx, userId)?.Tokens ?? 0;
        if (tx.Db.TokenLimit.Singleton.Find(true) is { } limit && used >= limit.TokensPerWindow)
            throw new Exception($"agent.rate_limited:{used}/{limit.TokensPerWindow}");
    }

    static void DemoBumpRateLimit(AgentsTx tx, string userId, ulong tokens)
    {
        if (tx.Db.TokenLimit.Singleton.Find(true) is not { } limit) return;
        var open = DemoOpenUsage(tx, userId);
        var row = new TokenUsage
        {
            Owner = userId,
            Tokens = (open?.Tokens ?? 0) + tokens,
            WindowEndsAt = open?.WindowEndsAt ?? new Timestamp(tx.Timestamp.MicrosecondsSinceUnixEpoch + limit.WindowSecs * 1_000_000L),
        };
        if (tx.Db.TokenUsage.Owner.Find(userId) is not null) tx.Db.TokenUsage.Owner.Update(row);
        else tx.Db.TokenUsage.Insert(row);
    }

    static List<LoopAttachment> DemoAttachmentsFor(AgentsTx tx, ulong messageId) =>
        tx.Db.MessageAttachment.MessageId.Filter(messageId)
            .OrderBy(r => r.Ordinal)
            .Select(row => tx.Db.File.Id.Find(row.FileId) is { } file && tx.Db.FileBlob.FileId.Find(row.FileId) is { } blob
                ? new LoopAttachment(file.MimeType, AuthCrypto.Base64Encode(blob.Bytes))
                : null)
            .OfType<LoopAttachment>()
            .ToList();

    static readonly string[] DemoAttachmentMimes = ["image/png", "image/jpeg", "image/webp", "image/gif"];

    static string? DemoAttachmentValidationError(List<SendAttachment> attachments)
    {
        if (attachments.Count > 4) return $"agent.too_many_attachments:{attachments.Count}/4";
        var total = 0;
        foreach (var a in attachments)
        {
            if (!DemoAttachmentMimes.Contains(a.MimeType)) return $"agent.unsupported_attachment_mime:{a.MimeType}";
            if (a.Bytes.Length > FileBytesMax) return $"agent.attachment_too_large:{a.Bytes.Length}/{FileBytesMax}";
            total += a.Bytes.Length;
            if (total > 12_000_000) return $"agent.attachments_too_large:{total}/12000000";
        }
        return null;
    }

    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx)
    {
        AuthInstall(ctx);
        AgentsInstall(ctx);
    }

    /// <summary>Admin-gated; both values set enables the cap, both unset removes it.</summary>
    [SpacetimeDB.Reducer]
    public static void SetTokenLimit(ReducerContext ctx, uint? tokensPerWindow, uint? windowSecs)
    {
        if (!DemoClient.IsAdmin(AgentsTx.Of(ctx))) throw new Exception("agent.not_authorized");
        if (tokensPerWindow is null && windowSecs is null)
        {
            ctx.Db.TokenLimit.Singleton.Delete(true);
            return;
        }
        if (tokensPerWindow is not (> 0) || windowSecs is not (> 0)) throw new Exception("agent.invalid_token_limit");
        var row = new TokenLimit { Singleton = true, TokensPerWindow = tokensPerWindow.Value, WindowSecs = windowSecs.Value, UpdatedAt = ctx.Timestamp };
        if (ctx.Db.TokenLimit.Singleton.Find(true) is not null) ctx.Db.TokenLimit.Singleton.Update(row);
        else ctx.Db.TokenLimit.Insert(row);
    }

    [SpacetimeDB.Procedure]
    public static Result<AgentConfigStatus, string> GetAgentConfigStatus(ProcedureContext ctx) => AgentsTry(() => ctx.WithTx(t =>
    {
        var tx = AgentsTx.Of(t);
        var status = DemoClient.Status(tx);
        var limit = tx.Db.TokenLimit.Singleton.Find(true);
        return new AgentConfigStatus
        {
            IsConfigured = status.ConfiguredProviders.Count > 0,
            StaleLockThresholdSecs = status.StaleLockThresholdSecs,
            RateLimitTokensPerWindow = limit?.TokensPerWindow,
            RateLimitWindowSecs = limit?.WindowSecs,
            Agents = status.Agents.Select(a => new AgentInfo { Name = a.Name, Model = a.Model, Models = a.Models }).ToList(),
            ConfiguredProviders = status.ConfiguredProviders,
        };
    }));

    [SpacetimeDB.Procedure]
    public static Result<ulong, string> StartThread(ProcedureContext ctx, string agentName, string? title, string? metadata) => AgentsTry(() => ctx.WithTx(t =>
    {
        var tx = AgentsTx.Of(t);
        return DemoClient.StartThread(tx, DemoRequireUserId(tx), agentName, title, metadata);
    }));

    [SpacetimeDB.Reducer]
    public static void UpdateThread(ReducerContext ctx, ulong threadId, string? title, string? modelOverride, string? metadata, bool clearTitle, bool clearModelOverride, bool clearMetadata)
    {
        var tx = AgentsTx.Of(ctx);
        DemoClient.UpdateThread(tx, DemoRequireUserId(tx), threadId, title, modelOverride, metadata, clearTitle, clearModelOverride, clearMetadata);
    }

    [SpacetimeDB.Reducer]
    public static void DeleteThread(ReducerContext ctx, ulong threadId)
    {
        var tx = AgentsTx.Of(ctx);
        var owner = DemoRequireUserId(tx);
        DemoClient.DeleteThread(tx, owner, threadId);
        foreach (var a in ctx.Db.MessageAttachment.ThreadId.Filter(threadId).ToList())
        {
            if (ctx.Db.File.Id.Find(a.FileId) is { } file) FilesDelete(FilesTx.Of(ctx), file.Path, owner);
            ctx.Db.MessageAttachment.Id.Delete(a.Id);
        }
    }

    static string DemoOwner(ProcedureContext ctx) => ctx.WithTx(t => DemoRequireUserId(AgentsTx.Of(t)));

    /// <summary>No-op if the thread already has a title.</summary>
    [SpacetimeDB.Procedure]
    public static Result<AgentsUnit, string> GenerateThreadTitle(ProcedureContext ctx, ulong threadId) =>
        AgentsTry(() => DemoClient.GenerateThreadTitle(ctx, DemoOwner(ctx), threadId));

    [SpacetimeDB.Reducer]
    public static void RequestCancel(ReducerContext ctx, ulong threadId)
    {
        var tx = AgentsTx.Of(ctx);
        DemoClient.RequestCancel(tx, DemoRequireUserId(tx), threadId);
    }

    [SpacetimeDB.Procedure]
    public static Result<AgentsUnit, string> SendMessage(ProcedureContext ctx, ulong threadId, string content, List<SendAttachment> attachments) => AgentsTry(() =>
    {
        if (content.Length == 0 && attachments.Count == 0) throw new Exception("agent.empty_message");
        if (DemoAttachmentValidationError(attachments) is { } e) throw new Exception(e);
        var owner = DemoOwner(ctx);
        DemoClient.SendMessage(ctx, owner, threadId, content, (tx, messageId) =>
        {
            for (var ordinal = 0; ordinal < attachments.Count; ordinal++)
            {
                var a = attachments[ordinal];
                var fileId = FilesUpload(new FilesTx(tx.Db, tx.Timestamp), new FilesUploadArgs($"/msg/{messageId}/{ordinal}", a.MimeType, a.Bytes, FileVisibilityOwner), owner);
                tx.Db.MessageAttachment.Insert(new MessageAttachment
                {
                    Id = 0,
                    FileId = fileId,
                    MessageId = messageId,
                    ThreadId = threadId,
                    OwnerUserId = owner,
                    Ordinal = (uint)ordinal,
                    Filename = a.Filename,
                    CreatedAt = tx.Timestamp,
                });
            }
        });
    });

    [SpacetimeDB.Procedure]
    public static Result<AgentsUnit, string> RegenerateResponse(ProcedureContext ctx, ulong threadId) =>
        AgentsTry(() => DemoClient.RegenerateResponse(ctx, DemoOwner(ctx), threadId));

    // -----------------------------------------------------------------------
    // The upstream example: views.ts
    // -----------------------------------------------------------------------

    static string? DemoCallerUserId(ViewContext ctx) => ctx.Db.AuthConnectionBinding.StdbIdentity.Find(ctx.Sender)?.UserId;

    [SpacetimeDB.View(Accessor = "MyThreads", Public = true)]
    public static List<AgentsThread> MyThreads(ViewContext ctx) =>
        DemoCallerUserId(ctx) is { } u ? ctx.Db.AgentsThread.Owner.Filter(u).ToList() : [];

    [SpacetimeDB.View(Accessor = "MyMessages", Public = true)]
    public static List<AgentsMessage> MyMessages(ViewContext ctx) =>
        DemoCallerUserId(ctx) is { } u ? ctx.Db.AgentsMessage.Owner.Filter(u).ToList() : [];

    [SpacetimeDB.View(Accessor = "MyThreadLocks", Public = true)]
    public static List<AgentsThreadLock> MyThreadLocks(ViewContext ctx) =>
        DemoCallerUserId(ctx) is { } u ? ctx.Db.AgentsThreadLock.Owner.Filter(u).ToList() : [];

    [SpacetimeDB.View(Accessor = "MyFiles", Public = true)]
    public static List<AttachmentFile> MyFiles(ViewContext ctx)
    {
        if (DemoCallerUserId(ctx) is not { } userId) return [];
        var rows = new List<AttachmentFile>();
        foreach (var a in ctx.Db.MessageAttachment.OwnerUserId.Filter(userId))
        {
            if (ctx.Db.File.Id.Find(a.FileId) is not { } file) continue;
            rows.Add(new AttachmentFile
            {
                Id = a.Id,
                FileId = a.FileId,
                Path = file.Path,
                OwnerUserId = a.OwnerUserId,
                MimeType = file.MimeType,
                Size = file.Size,
                Sha256Hex = file.Sha256Hex,
                Visibility = file.Visibility,
                Filename = a.Filename,
                MessageId = a.MessageId,
                ThreadId = a.ThreadId,
                Ordinal = a.Ordinal,
                CreatedAt = a.CreatedAt,
                UpdatedAt = file.UpdatedAt,
            });
        }
        return rows;
    }

    // -----------------------------------------------------------------------
    // HTTP routes (auth and owner-only attachment downloads)
    // -----------------------------------------------------------------------

    /// <summary>Local plain-HTTP development: no Secure cookies; the tests send x-real-ip.</summary>
    static AuthHttpOptions DemoAuthHttp => new()
    {
        TrustedProxyHeader = AuthTrustedProxyHeader.XRealIp,
        SecureCookies = false,
        SendMail = (_, mail) => Log.Info($"[mail] to={mail.To} subject={mail.Subject}\n{mail.Text}"),
    };

    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthPasswordSignup(HandlerContext ctx, HttpRequest req) => AuthPasswordSignup(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthPasswordLogin(HandlerContext ctx, HttpRequest req) => AuthPasswordLogin(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthMe(HandlerContext ctx, HttpRequest req) => AuthMe(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthLogout(HandlerContext ctx, HttpRequest req) => AuthLogout(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthRefresh(HandlerContext ctx, HttpRequest req) => AuthRefresh(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthGoogleStart(HandlerContext ctx, HttpRequest req) => AuthGoogleStart(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthGoogleCallback(HandlerContext ctx, HttpRequest req) => AuthGoogleCallback(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthGithubStart(HandlerContext ctx, HttpRequest req) => AuthGithubStart(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthGithubCallback(HandlerContext ctx, HttpRequest req) => AuthGithubCallback(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthPasswordForgot(HandlerContext ctx, HttpRequest req) => AuthForgotPassword(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthPasswordReset(HandlerContext ctx, HttpRequest req) => AuthResetPassword(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthEmailVerifyRequest(HandlerContext ctx, HttpRequest req) => AuthEmailVerifyRequest(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthEmailVerify(HandlerContext ctx, HttpRequest req) => AuthEmailVerify(ctx, req, DemoAuthHttp);

    /// <summary>Attachments are owner-only; the request's session cookie or bearer token identifies the owner.</summary>
    [SpacetimeDB.HttpHandler]
    public static HttpResponse RouteFileServe(HandlerContext ctx, HttpRequest req)
    {
        var userId = ctx.WithTx(tx => AuthRequestUserId(AuthTx.Of(tx), req));
        return FilesServe(ctx, req, f => userId is not null && f.OwnerUserId == userId);
    }

    [SpacetimeDB.HttpRouter]
    public static Router Routes() => Router.New()
        .Post("/auth/password/signup", Handlers.RouteAuthPasswordSignup)
        .Post("/auth/password/login", Handlers.RouteAuthPasswordLogin)
        .Post("/auth/session/refresh", Handlers.RouteAuthRefresh)
        .Get("/auth/me", Handlers.RouteAuthMe)
        .Post("/auth/logout", Handlers.RouteAuthLogout)
        .Get("/auth/google/start", Handlers.RouteAuthGoogleStart)
        .Get("/auth/google/callback", Handlers.RouteAuthGoogleCallback)
        .Get("/auth/github/start", Handlers.RouteAuthGithubStart)
        .Get("/auth/github/callback", Handlers.RouteAuthGithubCallback)
        .Post("/auth/password/forgot", Handlers.RouteAuthPasswordForgot)
        .Post("/auth/password/reset", Handlers.RouteAuthPasswordReset)
        .Post("/auth/email/verify-request", Handlers.RouteAuthEmailVerifyRequest)
        .Get("/auth/email/verify", Handlers.RouteAuthEmailVerify)
        .Get("/files", Handlers.RouteFileServe)
        .Get("/files/", Handlers.RouteFileServe)
        .Head("/files/", Handlers.RouteFileServe)
        .Head("/files", Handlers.RouteFileServe);

    // -----------------------------------------------------------------------
    // Test helpers: vectors from the TS code
    // -----------------------------------------------------------------------

    static AgentDefinition DemoKitchen => DemoClient.AgentDef("kitchen")!;

    /// <summary>The kitchen agent's tool definitions, as JSON.</summary>
    [SpacetimeDB.Procedure]
    public static string DemoToolDefs(ProcedureContext ctx) => AgentsJs.Stringify(DemoKitchen.Dispatch.LlmToolDefs);

    /// <summary>Invoke a kitchen tool: {"result", "isError"}.</summary>
    [SpacetimeDB.Procedure]
    public static string DemoInvoke(ProcedureContext ctx, string tool, string input)
    {
        var r = ctx.WithTx(tx => DemoKitchen.Dispatch.Invoke(AgentsTx.Of(tx), tool, input));
        return AgentsJs.Stringify(new JsObject { ["result"] = r.Result, ["isError"] = r.IsError });
    }

    static string DemoErr(Action f)
    {
        try
        {
            f();
            return "";
        }
        catch (Exception e)
        {
            return e.Message;
        }
    }

    static AgentDefinition DemoPlainAgent(string? summarizer = null) => AgentDefinition.Define(new AgentSpec { DefaultModel = "m", SummarizerAgentName = summarizer });

    /// <summary>The definition-time errors for the cases in the vectors.</summary>
    [SpacetimeDB.Procedure]
    public static string DemoDefinitionErrors(ProcedureContext ctx)
    {
        AgentTool Unit() => AgentTool.Create("ok", AgentArg.Unit, (_, _) => "");
        string Spec(Func<AgentSpec> f) => DemoErr(() => AgentDefinition.Define(f()));
        var cases = new JsObject
        {
            ["u128"] = DemoErr(() => AgentSchema.ToJsonSchema(AgentArg.Object(("x", AgentArg.U128)))),
            ["i128"] = DemoErr(() => AgentSchema.ToJsonSchema(AgentArg.Object(("x", AgentArg.Option(AgentArg.I128))))),
            ["top_string"] = DemoErr(() => AgentSchema.ToJsonSchema(AgentArg.String)),
            ["top_array"] = DemoErr(() => AgentSchema.ToJsonSchema(AgentArg.Array(AgentArg.String))),
            ["bad_tool_name"] = DemoErr(() => new AgentDispatch([("has spaces", Unit())])),
            ["long_tool_name"] = DemoErr(() => new AgentDispatch([(new string('a', 65), Unit())])),
            ["too_many_tools"] = DemoErr(() => new AgentDispatch(Enumerable.Range(0, 65).Select(i => ($"t{i}", Unit())))),
            ["description_empty"] = DemoErr(() => AgentTool.Create("   ", AgentArg.Unit, (_, _) => "")),
            ["description_long"] = DemoErr(() => AgentTool.Create(new string('d', 1025), AgentArg.Unit, (_, _) => "")),
            ["model_empty"] = Spec(() => new AgentSpec { DefaultModel = "  " }),
            ["model_long"] = Spec(() => new AgentSpec { DefaultModel = new string('m', 257) }),
            ["provider_long"] = Spec(() => new AgentSpec { DefaultModel = "m", DefaultProvider = new string('p', 65) }),
            ["prompt_long"] = Spec(() => new AgentSpec { DefaultModel = "m", DefaultSystemPrompt = new string('s', 32769) }),
            ["turns_zero"] = Spec(() => new AgentSpec { DefaultModel = "m", DefaultMaxTurns = 0 }),
            ["turns_big"] = Spec(() => new AgentSpec { DefaultModel = "m", DefaultMaxTurns = 101 }),
            ["history_big"] = Spec(() => new AgentSpec { DefaultModel = "m", DefaultMaxHistoryMessages = 1001 }),
            ["tokens_zero"] = Spec(() => new AgentSpec { DefaultModel = "m", DefaultMaxTokens = 0 }),
            ["retries_big"] = Spec(() => new AgentSpec { DefaultModel = "m", DefaultRetries = 11 }),
            ["rag_big"] = Spec(() => new AgentSpec { DefaultModel = "m", RagTopK = 101 }),
            ["models_empty"] = Spec(() => new AgentSpec { DefaultModel = "m", Models = [" "] }),
            ["summarizer_empty"] = Spec(() => new AgentSpec { DefaultModel = "m", SummarizerAgentName = "" }),
            ["embeddings_model_long"] = Spec(() => new AgentSpec { DefaultModel = "m", EmbeddingsModel = new string('e', 257) }),
            ["bad_agent_name"] = DemoErr(() => new AgentsClient(new AgentsConfig { Agents = [("a b", DemoPlainAgent())] })),
            ["unknown_summarizer"] = DemoErr(() => new AgentsClient(new AgentsConfig { Agents = [("chat", DemoPlainAgent("nobody"))] })),
            ["max_threads_zero"] = DemoErr(() => new AgentsClient(new AgentsConfig { Agents = [("chat", DemoPlainAgent())], MaxThreadsPerOwner = 0 })),
        };
        return AgentsJs.Stringify(cases);
    }

    /// <summary>defineAgent defaults and trimming, in the TS shape.</summary>
    [SpacetimeDB.Procedure]
    public static string DemoDefaults(ProcedureContext ctx)
    {
        var a = AgentDefinition.Define(new AgentSpec { DefaultModel = "  m/x  ", Models = [" a ", "b"], SummarizerAgentName = " s " });
        var o = new JsObject
        {
            ["defaultProvider"] = a.DefaultProvider,
            ["defaultModel"] = a.DefaultModel,
            ["models"] = new JsArray(a.Models),
        };
        if (a.DefaultSystemPrompt is { } p) o["defaultSystemPrompt"] = p;
        o["defaultMaxTurns"] = (double)a.DefaultMaxTurns;
        o["defaultMaxHistoryMessages"] = (double)a.DefaultMaxHistoryMessages;
        if (a.DefaultMaxTokens is { } t) o["defaultMaxTokens"] = (double)t;
        o["defaultRetries"] = (double)a.DefaultRetries;
        if (a.SummarizerAgentName is { } s) o["summarizerAgentName"] = s;
        o["ragTopK"] = (double)a.RagTopK;
        return AgentsJs.Stringify(o);
    }

    static JsArray DemoHeaders(List<(string Name, string Value)> headers) => new(headers.Select(h => (object?)new JsArray { h.Name, h.Value }));

    /// <summary>provider.buildRequest(request): {url, headers: [[k, v]], body}.</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoBuildRequest(ProcedureContext ctx, string provider, string requestJson) => AgentsTry(() =>
    {
        var p = AgentProvider.BuiltIn(provider) ?? throw new Exception("unknown provider");
        var b = p.BuildRequest(ChatRequest.FromJs(AgentsJs.Parse(requestJson)));
        return AgentsJs.Stringify(new JsObject { ["url"] = b.Url, ["headers"] = DemoHeaders(b.Headers), ["body"] = b.Body });
    });

    /// <summary>provider.parseResponse(text, 'requested-model'): {ok: response} or {error: message}.</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoParseResponse(ProcedureContext ctx, string provider, string text) => AgentsTry(() =>
    {
        var p = AgentProvider.BuiltIn(provider) ?? throw new Exception("unknown provider");
        try
        {
            return AgentsJs.Stringify(new JsObject { ["ok"] = p.ParseResponse(text, "requested-model").ToJs() });
        }
        catch (Exception e)
        {
            return AgentsJs.Stringify(new JsObject { ["error"] = e.Message });
        }
    });

    /// <summary>Replays canned responses: [status, body], or ["throw", message] for a transport error.</summary>
    sealed class DemoScriptedHttp(JsArray responses) : IAgentsHttp
    {
        public int Attempts;
        public JsObject? Sent;

        public (ushort Status, string Body) Post(string url, List<(string Name, string Value)> headers, string body)
        {
            Sent = new JsObject { ["url"] = url, ["headers"] = DemoHeaders(headers), ["body"] = body };
            var r = (JsArray)responses[Attempts++]!;
            var text = r[1] as string ?? "";
            return r[0] is double status ? ((ushort)status, text) : throw new Exception(text);
        }
    }

    /// <summary>callChat (OpenAI shape) over a scripted transport: {attempts, result}.</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoCallChat(ProcedureContext ctx, uint retries, string responsesJson) => AgentsTry(() =>
    {
        var http = new DemoScriptedHttp((JsArray)AgentsJs.Parse(responsesJson)!);
        var r = AgentsChat.Call(http, AgentProvider.BuiltIn("openai")!, new ChatRequest { ApiKey = "k", Model = "m", Messages = [new("user", "hi")], Retries = retries });
        var result = r.Response is { } resp ? new JsObject { ["ok"] = true, ["text"] = resp.Text } : new JsObject { ["ok"] = false, ["error"] = r.Error!.ToJs() };
        return AgentsJs.Stringify(new JsObject { ["attempts"] = (double)http.Attempts, ["result"] = result });
    });

    /// <summary>An embeddings provider over a scripted transport: {sent, result} in the TS shapes.</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoEmbed(ProcedureContext ctx, string provider, string textsJson, ushort status, string body) => AgentsTry(() =>
    {
        var texts = ((JsArray)AgentsJs.Parse(textsJson)!).Select(t => (string)t!).ToList();
        var url = AgentProvider.DefaultUrl($"{provider}.embeddings") ?? throw new Exception("unknown provider");
        var http = new DemoScriptedHttp(new JsArray { new JsArray { (double)status, body } });
        var r = AgentsChat.Embed(http, url, "ek", "emb-model", texts);
        var result = r.Ok
            ? new JsObject
            {
                ["ok"] = true,
                ["vectors"] = new JsArray(r.Vectors!.Select(v => (object?)new JsArray(v.Select(x => (object?)x)))),
                ["model"] = r.Model,
                ["usage"] = new JsObject { ["promptTokens"] = (double)r.PromptTokens, ["totalTokens"] = (double)r.TotalTokens },
            }
            : new JsObject { ["ok"] = false, ["error"] = r.Error!.ToJs() };
        return AgentsJs.Stringify(new JsObject { ["sent"] = http.Sent, ["result"] = result });
    });

    static List<double> DemoNumbers(string json) => ((JsArray)AgentsJs.Parse(json)!).Select(x => (double)x!).ToList();

    [SpacetimeDB.Procedure]
    public static Result<double, string> DemoCosine(ProcedureContext ctx, string aJson, string bJson) =>
        AgentsTry(() => AgentsChat.CosineSimilarity(DemoNumbers(aJson), DemoNumbers(bJson)));

    /// <summary>Indices of the k best scores, best first.</summary>
    [SpacetimeDB.Procedure]
    public static Result<List<uint>, string> DemoTopK(ProcedureContext ctx, string scoresJson, uint k) => AgentsTry(() =>
    {
        var scores = DemoNumbers(scoresJson);
        return AgentsChat.TopKByScore(Enumerable.Range(0, scores.Count).Select(i => (uint)i), i => scores[(int)i], (int)k).Select(x => x.Item).ToList();
    });

    static List<LoopMessage> DemoLoopMessages(string json) =>
        ((JsArray)AgentsJs.Parse(json)!).Select(r => new LoopMessage(
            (ulong)(double)AgentsJs.Get(r, "id")!,
            (string)AgentsJs.Get(r, "role")!,
            (string)AgentsJs.Get(r, "content")!,
            AgentsJs.Get(r, "toolCallsJson") as string,
            AgentsJs.Get(r, "toolCallId") as string,
            AgentsJs.Get(r, "attachments") is JsArray a ? a.Select(x => new LoopAttachment((string)AgentsJs.Get(x, "mimeType")!, (string)AgentsJs.Get(x, "data")!)).ToList() : [])).ToList();

    /// <summary>buildLlmMessages over the given rows.</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoHistory(ProcedureContext ctx, string messagesJson, uint maxHistory) => AgentsTry(() =>
        AgentsJs.Stringify(new JsArray(AgentsHistory.BuildLlmMessages(DemoLoopMessages(messagesJson), maxHistory).Select(m => (object?)m.ToJs()))));

    /// <summary>pickSummarizationCandidates + buildSummarizerUserContent: {lastNewId, content}.</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoSummary(ProcedureContext ctx, string messagesJson, uint maxHistory, ulong? summarizedThroughId, string? existing) => AgentsTry(() =>
    {
        var picked = AgentsHistory.PickSummarizationCandidates(DemoLoopMessages(messagesJson), (int)maxHistory, summarizedThroughId);
        return AgentsJs.Stringify(picked is { } p
            ? new JsObject { ["lastNewId"] = (double)p.LastNewId, ["content"] = AgentsHistory.SummarizerUserContent(existing, p.Dropped) }
            : new JsObject { ["lastNewId"] = null, ["content"] = null });
    });

    /// <summary>buildSummarizerUserContent(null, rows).</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoSummarizerContent(ProcedureContext ctx, string messagesJson) =>
        AgentsTry(() => AgentsHistory.SummarizerUserContent(null, DemoLoopMessages(messagesJson)));

    [SpacetimeDB.Procedure]
    public static string? DemoContext(ProcedureContext ctx, string? summary, List<string> snippets) => AgentsHistory.ContextMessage(summary, snippets);

    // -----------------------------------------------------------------------
    // Test helpers: locks
    // -----------------------------------------------------------------------

    /// <summary>Takes a thread's lock as if a run were in progress (any thread, for the sweep tests).</summary>
    [SpacetimeDB.Reducer]
    public static void DemoLockThread(ReducerContext ctx, ulong threadId)
    {
        if (!DemoClient.IsAdmin(AgentsTx.Of(ctx))) throw new Exception("agent.not_authorized");
        var owner = ctx.Db.AgentsThread.Id.Find(threadId)?.Owner ?? "";
        ctx.Db.AgentsThreadLock.Insert(new AgentsThreadLock { ThreadId = threadId, Owner = owner, LockedAt = ctx.Timestamp, CancelRequested = false });
    }

    /// <summary>Runs the stale-lock sweep with a batch size; returns how many locks it deleted.</summary>
    [SpacetimeDB.Procedure]
    public static Result<uint, string> DemoSweep(ProcedureContext ctx, uint maxRows) => AgentsTry(() => ctx.WithTx(t =>
    {
        var tx = AgentsTx.Of(t);
        if (!DemoClient.IsAdmin(tx)) throw new Exception("agent.not_authorized");
        return (uint)AgentsSweepStaleLocks(tx, (int)maxRows);
    }));
}
