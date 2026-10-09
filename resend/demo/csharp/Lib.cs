// Demo for Resend.cs: the standalone package's procedures and reducers, the webhook route at
// /webhook/resend, and the upstream example's pattern of a host send procedure plus caller-scoped
// history. The C# twin of demo/rust.
//
// Build with RESEND_API_BASE=http://127.0.0.1:4201 to send to the test suite's mock API.

#pragma warning disable STDB_UNSTABLE

using SpacetimeDB;

public static partial class Module
{
    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx) => ResendInstall(ctx);

    [SpacetimeDB.HttpRouter]
    public static Router Routes() => Router.New().Post("/webhook/resend", Handlers.ResendWebhook);

    /// <summary>
    /// The host decides who may send: here anyone may email themselves a welcome note (the address
    /// is checked and the history is tagged with the caller), like the upstream sendDispatch.
    /// </summary>
    [SpacetimeDB.Procedure]
    public static Result<ResendSendEmailResult, string> SendWelcome(ProcedureContext ctx, string to)
    {
        to = to.Trim().ToLowerInvariant();
        if (to.Split('@').Length != 2 || to.Any(char.IsWhiteSpace)) return Result<ResendSendEmailResult, string>.Err("dispatch.invalid_email");
        var userId = ctx.Sender.ToString().ToLowerInvariant();
        try
        {
            return Result<ResendSendEmailResult, string>.Ok(ResendSendEmailRequest(ctx, new ResendSendEmailArgs
            {
                To = [to],
                Subject = "Welcome",
                Html = "<p>Your workspace is ready.</p>",
                TagsJson = $"[{{\"name\":\"userId\",\"value\":\"{userId}\"}}]",
            }));
        }
        catch (Exception e)
        {
            return Result<ResendSendEmailResult, string>.Err(e.Message);
        }
    }

    /// <summary>
    /// The caller's own emails (Resend's tables stay private). A procedure, like the Rust demo:
    /// views can only read indexes, and the user_id index is on an optional column.
    /// </summary>
    [SpacetimeDB.Procedure]
    public static List<ResendEmail> MyEmails(ProcedureContext ctx)
    {
        var me = ctx.Sender.ToString().ToLowerInvariant();
        return ctx.WithTx(tx => tx.Db.ResendEmail.Iter().Where(e => e.UserId == me).ToList());
    }
}
