// Demo for Retry.cs: the upstream example's `flaky` task plus a few tasks the test suite uses.
// The C# twin of demo/rust.

#pragma warning disable STDB_UNSTABLE

using SpacetimeDB;

[SpacetimeDB.Type]
public partial struct FlakyArgs
{
    public string TaskName;
    public byte SucceedAtAttempt;
    public bool ThrowOnFailure;
}

/// <summary>One variant per task, named like the TS task tags.</summary>
[SpacetimeDB.Type]
public partial record RetryArgs : TaggedEnum<(FlakyArgs flaky, Unit fails, Unit rejects, Unit longError, Unit succeeds)>;

public static partial class Module
{
    [SpacetimeDB.Table(Accessor = "RetryMetric", Public = true)]
    public partial struct RetryMetric
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        public string Name;
        public double Value;
        public Timestamp RecordedAt;
    }

    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx) => RetryInstall(ctx);

    /// <summary>The one handler Retry.cs calls, dispatching on the task's arguments.</summary>
    public static partial RetryResult RetryHandle(ReducerContext ctx, RetryArgs args)
    {
        switch (args)
        {
            case RetryArgs.flaky(var a):
                // The attempt being run is still in retry_task while its reducer runs.
                var attempt = ctx.Db.RetryTask.Name.Filter(a.TaskName).FirstOrDefault().Attempt;
                if (attempt < a.SucceedAtAttempt)
                {
                    var message = $"simulated failure at attempt {attempt}";
                    if (a.ThrowOnFailure) throw new Exception(message);
                    return RetryResult.Failed(message);
                }
                ctx.Db.RetryMetric.Insert(new RetryMetric
                {
                    Id = 0,
                    Name = $"flaky-success-{a.TaskName}",
                    Value = attempt,
                    RecordedAt = ctx.Timestamp,
                });
                return RetryResult.Ok();
            case RetryArgs.fails:
                return RetryResult.Failed("unavailable");
            case RetryArgs.rejects:
                throw new RetryPermanentException("order.not_found");
            case RetryArgs.longError:
                return RetryResult.Failed(new string('x', 3000));
            default:
                return RetryResult.Ok();
        }
    }

    /// <summary>Test helper: run `count` successful attempts inline (exercises the 1,000-row history cap).</summary>
    [SpacetimeDB.Reducer]
    public static void DemoRunInline(ReducerContext ctx, uint count)
    {
        RetryRequireAdmin(ctx);
        var task = new RetryTask
        {
            ScheduledId = 0,
            ScheduledAt = new ScheduleAt.Time(ctx.Timestamp),
            Name = "inline",
            Args = new RetryArgs.succeeds(default),
            Attempt = 0,
            MaxAttempts = 1,
            BackoffSecs = 1,
        };
        for (var i = 0; i < count; i++) RetryRunAttempt(ctx, task);
    }
}
