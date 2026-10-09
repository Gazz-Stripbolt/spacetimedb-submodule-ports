// Demo for Stripe.cs: the standalone package's procedures and reducers, the webhook route at
// /stripe/webhook, and the upstream "Premium Store" pattern: a server-owned product catalog and a
// host checkout procedure any signed-in user may call. The C# twin of demo/rust.
//
// Build with STRIPE_API_BASE=http://127.0.0.1:4202 to talk to the test suite's mock API.

#pragma warning disable STDB_UNSTABLE

using SpacetimeDB;

[SpacetimeDB.Type]
public partial struct StoreCheckoutSession
{
    public string? Url;
}

[SpacetimeDB.Type]
public partial struct DemoStripeResponse
{
    public ushort Status;
    public string Body;
}

public static partial class Module
{
    /// <summary>The server-owned catalog: clients pick a product, never a price.</summary>
    [SpacetimeDB.Table(Accessor = "StoreProduct")]
    public partial struct StoreProduct
    {
        [SpacetimeDB.PrimaryKey]
        public string ProductId;
        public string? StripePriceId;
        public bool Active;
    }

    const string AppOrigin = "https://store.example.com";

    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx)
    {
        StripeInstall(ctx);
        ctx.Db.StoreProduct.Insert(new StoreProduct { ProductId = "premium", StripePriceId = "price_premium", Active = true });
        ctx.Db.StoreProduct.Insert(new StoreProduct { ProductId = "retired", StripePriceId = "price_old", Active = false });
    }

    [SpacetimeDB.HttpRouter]
    public static Router Routes() => Router.New().Post("/stripe/webhook", Handlers.StripeWebhook);

    /// <summary>Anyone may start Checkout for an active product, as themselves.</summary>
    [SpacetimeDB.Procedure]
    public static Result<StoreCheckoutSession, string> CreateStoreCheckoutSession(ProcedureContext ctx, string productId)
    {
        var priceId = ctx.WithTx(tx => tx.Db.StoreProduct.ProductId.Find(productId) is { Active: true } p ? p.StripePriceId : null);
        if (priceId is null) return Result<StoreCheckoutSession, string>.Err("store.product_unavailable");
        try
        {
            var session = StripeCreateUserCheckoutSession(ctx, new StripeUserCheckoutArgs
            {
                UserId = ctx.Sender.ToString().ToLowerInvariant(),
                Items = [new StripeCheckoutLineItem { PriceId = priceId, Quantity = 1 }],
                Mode = "payment",
                SuccessUrl = $"{AppOrigin}/?checkout=success",
                CancelUrl = $"{AppOrigin}/?checkout=cancelled",
            });
            return Result<StoreCheckoutSession, string>.Ok(new StoreCheckoutSession { Url = session.Url });
        }
        catch (Exception e)
        {
            return Result<StoreCheckoutSession, string>.Err(e.Message);
        }
    }

    /// <summary>The caller's own paid Checkout sessions (fulfil from webhook state, not the redirect).</summary>
    [SpacetimeDB.Procedure]
    public static List<string> MyPaidSessions(ProcedureContext ctx)
    {
        var needle = $"\"userId\":\"{ctx.Sender.ToString().ToLowerInvariant()}\"";
        return ctx.WithTx(tx => tx.Db.StripeCheckoutSession.Iter()
            .Where(s => s.PaymentStatus is "paid" or "no_payment_required")
            .Where(s => s.MetadataJson?.Contains(needle) == true)
            .Select(s => s.StripeCheckoutSessionId)
            .ToList());
    }

    /// <summary>Admins only: the StripeRequest host helper (a relative /v1/ path with the stored key).</summary>
    [SpacetimeDB.Procedure]
    public static Result<DemoStripeResponse, string> DemoStripeRequest(ProcedureContext ctx, string method, string path, string? formBody, string? idempotencyKey)
    {
        var sender = ctx.Sender;
        if (!ctx.WithTx(tx => StripeIsAdmin(tx.Db, sender))) return Result<DemoStripeResponse, string>.Err(StripeErrors.NotAuthorized);
        try
        {
            var r = StripeRequest(ctx, method, path, formBody, idempotencyKey);
            return Result<DemoStripeResponse, string>.Ok(new DemoStripeResponse { Status = r.Status, Body = r.Body });
        }
        catch (Exception e)
        {
            return Result<DemoStripeResponse, string>.Err(e.Message);
        }
    }

    /// <summary>Test hook: run the retention sweep as of a given time (the scheduled reducer rejects clients).</summary>
    [SpacetimeDB.Reducer]
    public static void DemoPruneWebhookEventsAt(ReducerContext ctx, long nowMicros)
    {
        if (!StripeIsAdmin(ctx.Db, ctx.Sender)) throw new Exception(StripeErrors.NotAuthorized);
        StripePruneWebhookEventsAt(ctx.Db, new Timestamp(nowMicros));
    }
}
