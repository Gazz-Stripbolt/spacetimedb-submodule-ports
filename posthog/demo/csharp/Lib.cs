// Demo for Posthog.cs: the standalone package's procedures, reducers and views, plus the README's
// host pattern: an application reducer that records an analytics event in the same transaction as
// its own change. Test hooks pause the scheduler and run retention at a given time.

using SpacetimeDB;

public static partial class Module
{
    [SpacetimeDB.Table(Accessor = "DemoOrder")]
    public partial struct DemoOrder
    {
        [SpacetimeDB.PrimaryKey]
        public string OrderId;
        public ulong TotalCents;
    }

    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx) => PosthogInstall(ctx);

    /// <summary>
    /// Any user: completes an order and queues order_completed in the same transaction. Bad
    /// analytics input never rolls the order back (the event is stored as Rejected instead).
    /// </summary>
    [SpacetimeDB.Reducer]
    public static void CompleteOrder(ReducerContext ctx, string orderId, ulong totalCents, string? propertiesOverride)
    {
        ctx.Db.DemoOrder.Insert(new DemoOrder { OrderId = orderId, TotalCents = totalCents });
        PosthogEnqueueEventInTx(ctx, new PosthogEnqueueEventArgs
        {
            DistinctId = ctx.Sender.ToString().ToLowerInvariant(),
            Event = "order_completed",
            PropertiesJson = propertiesOverride ?? $"{{\"orderId\":\"{orderId}\",\"totalCents\":\"{totalCents}\"}}",
            IdempotencyKey = $"order_completed:{orderId}",
        });
    }

    static void DemoRequireAdmin(ReducerContext ctx)
    {
        if (!PosthogIsAdmin(ctx.Db, ctx.Sender)) throw new Exception(PosthogErrors.NotAuthorized);
    }

    /// <summary>Test hook: stop the 5-second flush so tests can drive delivery by hand.</summary>
    [SpacetimeDB.Reducer]
    public static void DemoPauseScheduler(ReducerContext ctx)
    {
        DemoRequireAdmin(ctx);
        foreach (var id in ctx.Db.PosthogFlushTick.Iter().Select(t => t.ScheduledId).ToList())
        {
            ctx.Db.PosthogFlushTick.ScheduledId.Delete(id);
        }
    }

    /// <summary>Test hook: restart it (what PosthogInstall does).</summary>
    [SpacetimeDB.Reducer]
    public static void DemoResumeScheduler(ReducerContext ctx)
    {
        DemoRequireAdmin(ctx);
        PosthogInstall(ctx);
    }

    /// <summary>Test hook: run the retention sweep as of nowMicros.</summary>
    [SpacetimeDB.Reducer]
    public static void DemoPruneHistoryAt(ReducerContext ctx, long nowMicros)
    {
        DemoRequireAdmin(ctx);
        PosthogPruneDeliveryHistory(ctx.Db, new Timestamp(nowMicros));
    }
}
