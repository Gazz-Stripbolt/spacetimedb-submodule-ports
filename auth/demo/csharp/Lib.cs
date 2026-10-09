// Demo for Auth.cs: the upstream example module (private notes behind sign-in, every auth route)
// plus a mock OAuth provider pair and procedures the test suite uses to check the port's crypto
// against vectors from the TS code. The C# twin of auth/demo/rust.

#pragma warning disable STDB_UNSTABLE

using System.Text;
using SpacetimeDB;

[SpacetimeDB.Type]
public partial struct DemoKeyInfo
{
    public string PublicHex;
    public string PrivateKeyPem;
    public string PublicKeyPem;
    public string Kid;
}

public static partial class Module
{
    [SpacetimeDB.Table(Accessor = "Note")]
    public partial struct Note
    {
        [SpacetimeDB.PrimaryKey]
        public string NoteId;
        [SpacetimeDB.Index.BTree]
        public string AuthorId;
        public string Title;
        public string Body;
        [SpacetimeDB.Index.BTree]
        public Timestamp CreatedAt;
    }

    /// <summary>Mail the demo "sends" (the upstream example logs it; tests read it back).</summary>
    [SpacetimeDB.Table(Accessor = "DemoMail")]
    public partial struct DemoMail
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        public string To;
        public string Subject;
        public string Text;
    }

    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx) => AuthInstall(ctx);

    [SpacetimeDB.View(Accessor = "MyNotes", Public = true)]
    public static List<Note> MyNotes(ViewContext ctx) =>
        ctx.Db.AuthConnectionBinding.StdbIdentity.Find(ctx.Sender) is { } b ? ctx.Db.Note.AuthorId.Filter(b.UserId).ToList() : [];

    [SpacetimeDB.Reducer]
    public static void CreateNote(ReducerContext ctx, string title, string body)
    {
        var userId = AuthRequireCallerUserId(ctx);
        ctx.Db.Note.Insert(new Note { NoteId = ctx.NewUuidV7().ToString(), AuthorId = userId, Title = title, Body = body, CreatedAt = ctx.Timestamp });
    }

    [SpacetimeDB.Reducer]
    public static void DeleteNote(ReducerContext ctx, string noteId)
    {
        var userId = AuthRequireCallerUserId(ctx);
        var row = ctx.Db.Note.NoteId.Find(noteId) ?? throw new Exception("note.not_found");
        if (row.AuthorId != userId) throw new Exception("note.not_owner");
        ctx.Db.Note.NoteId.Delete(noteId);
    }

    [SpacetimeDB.Reducer]
    public static void UpdateNote(ReducerContext ctx, string noteId, string title, string body)
    {
        var userId = AuthRequireCallerUserId(ctx);
        var row = ctx.Db.Note.NoteId.Find(noteId) ?? throw new Exception("note.not_found");
        if (row.AuthorId != userId) throw new Exception("note.not_owner");
        ctx.Db.Note.NoteId.Update(row with { Title = title, Body = body });
    }

    // -----------------------------------------------------------------------
    // HTTP routes
    // -----------------------------------------------------------------------

    static void DemoStoreMail(HandlerContext ctx, AuthMailParams mail)
    {
        Log.Info($"[mail] to={mail.To} subject={mail.Subject}\n{mail.Text}");
        ctx.WithTx(tx => tx.Db.DemoMail.Insert(new DemoMail { Id = 0, To = mail.To, Subject = mail.Subject, Text = mail.Text }));
    }

    /// <summary>Local plain-HTTP development: no Secure cookies; the tests send x-real-ip.</summary>
    static readonly AuthHttpOptions DemoAuthHttp = new()
    {
        TrustedProxyHeader = AuthTrustedProxyHeader.XRealIp,
        SecureCookies = false,
        SendMail = DemoStoreMail,
        AppName = "Notes",
        EmailVerifiedRedirect = "/?verified=1",
    };

    /// <summary>Test doubles for Google and GitHub, served by the test suite's mock provider.</summary>
    static OAuthProviderSpec DemoMock => AuthGoogle with
    {
        Id = "mock",
        AuthorizeUrl = "http://127.0.0.1:4310/authorize",
        TokenUrl = "http://127.0.0.1:4310/token",
        UserInfoUrl = "http://127.0.0.1:4310/userinfo",
    };
    static OAuthProviderSpec DemoMockGh => AuthGithub with
    {
        Id = "mockgh",
        AuthorizeUrl = "http://127.0.0.1:4310/gh/authorize",
        TokenUrl = "http://127.0.0.1:4310/gh/token",
        UserInfoUrl = "http://127.0.0.1:4310/gh/user",
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
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthMockStart(HandlerContext ctx, HttpRequest req) => AuthOauthStart(ctx, req, DemoAuthHttp, DemoMock);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthMockCallback(HandlerContext ctx, HttpRequest req) => AuthOauthCallback(ctx, req, DemoAuthHttp, DemoMock);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthMockghStart(HandlerContext ctx, HttpRequest req) => AuthOauthStart(ctx, req, DemoAuthHttp, DemoMockGh);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthMockghCallback(HandlerContext ctx, HttpRequest req) => AuthOauthCallback(ctx, req, DemoAuthHttp, DemoMockGh);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthPasswordForgot(HandlerContext ctx, HttpRequest req) => AuthForgotPassword(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthPasswordReset(HandlerContext ctx, HttpRequest req) => AuthResetPassword(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthEmailVerifyRequest(HandlerContext ctx, HttpRequest req) => AuthEmailVerifyRequest(ctx, req, DemoAuthHttp);
    [SpacetimeDB.HttpHandler] public static HttpResponse RouteAuthEmailVerify(HandlerContext ctx, HttpRequest req) => AuthEmailVerify(ctx, req, DemoAuthHttp);

    /// <summary>GET /whoami-http: the user id behind the request's bearer token or cookie (requestUserId).</summary>
    [SpacetimeDB.HttpHandler]
    public static HttpResponse RouteWhoamiHttp(HandlerContext ctx, HttpRequest req)
    {
        var user = ctx.WithTx(tx => AuthRequestUserId(AuthTx.Of(tx), req));
        return AuthJsonResponse($"{{\"userId\":{(user is null ? "null" : AuthJs.Str(user))}}}");
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
        .Get("/auth/mock/start", Handlers.RouteAuthMockStart)
        .Get("/auth/mock/callback", Handlers.RouteAuthMockCallback)
        .Get("/auth/mockgh/start", Handlers.RouteAuthMockghStart)
        .Get("/auth/mockgh/callback", Handlers.RouteAuthMockghCallback)
        .Post("/auth/password/forgot", Handlers.RouteAuthPasswordForgot)
        .Post("/auth/password/reset", Handlers.RouteAuthPasswordReset)
        .Post("/auth/email/verify-request", Handlers.RouteAuthEmailVerifyRequest)
        .Get("/auth/email/verify", Handlers.RouteAuthEmailVerify)
        .Get("/whoami-http", Handlers.RouteWhoamiHttp);

    // -----------------------------------------------------------------------
    // Vector checks (pure functions)
    // -----------------------------------------------------------------------

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

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoHashPassword(ProcedureContext ctx, string password, string saltHex) =>
        DemoTry(() => AuthCrypto.HashPassword(password, Crypto.HexToBytes(saltHex)));

    [SpacetimeDB.Procedure]
    public static Result<bool, string> DemoVerifyPassword(ProcedureContext ctx, string password, string encoded) =>
        DemoTry(() => AuthCrypto.VerifyPassword(password, encoded));

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoDummyHash(ProcedureContext ctx) => DemoTry(() => AuthCrypto.DummyPasswordHash);

    [SpacetimeDB.Procedure]
    public static Result<DemoKeyInfo, string> DemoKeypair(ProcedureContext ctx, string privateHex) => DemoTry(() =>
    {
        var kp = AuthKeypair.FromPrivateKeyBytes(Crypto.HexToBytes(privateHex));
        return new DemoKeyInfo { PublicHex = Crypto.BytesToHex(kp.PublicKey), PrivateKeyPem = kp.PrivateKeyPem, PublicKeyPem = kp.PublicKeyPem, Kid = kp.Kid };
    });

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoPrivateKeyFromPem(ProcedureContext ctx, string pem) =>
        DemoTry(() => Crypto.BytesToHex(AuthKeypair.PrivateKeyFromPem(pem)));

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoPublicKeyFromPem(ProcedureContext ctx, string pem) =>
        DemoTry(() => Crypto.BytesToHex(AuthKeypair.PublicKeyFromPem(pem)));

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoSessionJwt(ProcedureContext ctx, string privateHex, string iss, string sub, long iat, long exp, string jti, string? kid) =>
        DemoTry(() => AuthJwt.Sign(Crypto.HexToBytes(privateHex), AuthJwt.SessionClaims(iss, sub, iat, exp, jti), kid));

    /// <summary>{"ok":true,"claims":..,"header":..} or {"ok":false,"reason":..}, like TS verifyJwt.</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoVerifyJwt(ProcedureContext ctx, string publicHex, string token, string? issuer, string? audience, long nowSeconds) => DemoTry(() =>
    {
        var r = AuthJwt.Verify(Crypto.HexToBytes(publicHex), token, nowSeconds, issuer, audience);
        return r.Ok
            ? $"{{\"ok\":true,\"claims\":{r.Claims.GetRawText()},\"header\":{r.Header.GetRawText()}}}"
            : $"{{\"ok\":false,\"reason\":{AuthJs.Str(r.Reason!)}}}";
    });

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoDeriveSecret(ProcedureContext ctx, string keyHex, ulong counter, long micros) =>
        DemoTry(() => Crypto.BytesToHex(AuthCrypto.DeriveSecret(Crypto.HexToBytes(keyHex), counter, micros)));

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoUuid(ProcedureContext ctx, ulong ms, string randHex) =>
        DemoTry(() => AuthCrypto.UuidV7(ms, Crypto.HexToBytes(randHex)));

    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoPkce(ProcedureContext ctx, string verifier) => DemoTry(() => AuthCrypto.PkceChallenge(verifier));

    [SpacetimeDB.Procedure]
    public static Result<string?, string> DemoSafeRedirect(ProcedureContext ctx, string value) => DemoTry(() => AuthSafeRedirectPath(value));

    /// <summary>The parsed query as a JSON object (later keys win, like the TS object).</summary>
    [SpacetimeDB.Procedure]
    public static Result<string, string> DemoParseQuery(ProcedureContext ctx, string uri) => DemoTry(() =>
        "{" + string.Join(",", AuthParseQueryString(uri).Select(kv => $"{AuthJs.Str(kv.Key)}:{AuthJs.Str(kv.Value)}")) + "}");
}
