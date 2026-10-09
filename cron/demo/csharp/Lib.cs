// Demo for Cron.cs: the upstream demo module's jobs and management reducers, plus procedures
// that expose the expression parser and time-zone table to the test suite. The C# twin of
// demo/rust.

#pragma warning disable STDB_UNSTABLE

using SpacetimeDB;

[SpacetimeDB.Type]
public partial struct ReportCronArgs
{
    public string Label;
    public uint BatchSize;
}

[SpacetimeDB.Type]
public partial struct ProbeCronArgs
{
    public string Source;
}

/// <summary>One variant per cron job; the variant name is the job name.</summary>
[SpacetimeDB.Type]
public partial record CronJobArgsValue : TaggedEnum<(Unit heartbeat, ReportCronArgs report, Unit flaky, ProbeCronArgs probe)>;

[SpacetimeDB.Type]
public partial struct CronCase
{
    public string Expression;
    public long AfterMicros;
    public string Timezone;
}

[SpacetimeDB.Type]
public partial struct TzCase
{
    public string Timezone;
    public long UnixSeconds;
}

public static partial class Module
{
    public static partial CronSettings CronConfigure() => new(
        Jobs: [CronJobSpec.Reducer("heartbeat"), CronJobSpec.Reducer("report"), CronJobSpec.Reducer("flaky"), CronJobSpec.Procedure("probe")],
        HistoryCap: 5,
        ReconcileEverySeconds: 2);

    [SpacetimeDB.Table(Accessor = "TickLog", Public = true)]
    public partial struct TickLog
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        [SpacetimeDB.Index.BTree]
        public string JobName;
        public Timestamp At;
    }

    [SpacetimeDB.Table(Accessor = "FlakyState", Public = true)]
    public partial struct FlakyState
    {
        [SpacetimeDB.PrimaryKey]
        public bool Singleton;
        public bool Failing;
    }

    [SpacetimeDB.Table(Accessor = "ArgumentLog", Public = true)]
    public partial struct ArgumentLog
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        [SpacetimeDB.Index.BTree]
        public string JobName;
        public string Value;
        public uint Count;
        public string InvocationId;
    }

    /// <summary>Reducer jobs.</summary>
    public static partial void CronRunJob(ReducerContext ctx, CronJobArgsValue job, CronInvocation inv)
    {
        switch (job)
        {
            case CronJobArgsValue.heartbeat:
                ctx.Db.TickLog.Insert(new TickLog { Id = 0, JobName = "heartbeat", At = ctx.Timestamp });
                break;
            case CronJobArgsValue.report(var args):
                ctx.Db.TickLog.Insert(new TickLog { Id = 0, JobName = "report", At = ctx.Timestamp });
                ctx.Db.ArgumentLog.Insert(new ArgumentLog { Id = 0, JobName = "report", Value = args.Label, Count = args.BatchSize, InvocationId = inv.Id });
                break;
            case CronJobArgsValue.flaky:
                // Written, then rolled back if the job fails.
                ctx.Db.TickLog.Insert(new TickLog { Id = 0, JobName = "flaky", At = ctx.Timestamp });
                if (ctx.Db.FlakyState.Singleton.Find(true) is { Failing: true }) throw new Exception("flaky.failure");
                break;
        }
    }

    /// <summary>Procedure jobs: the natural home for side effects such as outbound HTTP.</summary>
    public static partial void CronRunProcedureJob(ProcedureContext ctx, CronJobArgsValue job, CronInvocation inv)
    {
        if (job is not CronJobArgsValue.probe(var args)) return;
        var failing = ctx.WithTx(tx => tx.Db.FlakyState.Singleton.Find(true) is { Failing: true });
        if (failing) throw new Exception("probe.failure");
        ctx.WithTx(tx =>
        {
            tx.Db.TickLog.Insert(new TickLog { Id = 0, JobName = "probe", At = tx.Timestamp });
            tx.Db.ArgumentLog.Insert(new ArgumentLog { Id = 0, JobName = "probe", Value = args.Source, Count = 0, InvocationId = inv.Id });
            return 0;
        });
    }

    [SpacetimeDB.View(Accessor = "CronJobs", Public = true)]
    public static List<CronJobView> CronJobs(ViewContext ctx) => CronPublicJobs(ctx);

    [SpacetimeDB.Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx)
    {
        ctx.Db.FlakyState.Insert(new FlakyState { Singleton = true, Failing = false });
        CronScheduleJob(ctx, new CronJobArgsValue.heartbeat(default), CronScheduleSpec.Every(30));
    }

    static CronJobArgsValue Argumentless(string name) => name switch
    {
        "heartbeat" => new CronJobArgsValue.heartbeat(default),
        "flaky" => new CronJobArgsValue.flaky(default),
        _ => throw new Exception($"test.unknown_job:{name}"),
    };

    [SpacetimeDB.Reducer]
    public static void ScheduleCron(ReducerContext ctx, string name, string expression, string timezone, uint maxFailures) =>
        CronScheduleJob(ctx, Argumentless(name), CronScheduleSpec.Cron(expression), new CronScheduleOpts { Timezone = timezone, MaxFailures = maxFailures });

    [SpacetimeDB.Reducer]
    public static void ScheduleEvery(ReducerContext ctx, string name, uint seconds, uint maxFailures) =>
        CronScheduleJob(ctx, Argumentless(name), CronScheduleSpec.Every(seconds), new CronScheduleOpts { MaxFailures = maxFailures });

    [SpacetimeDB.Reducer]
    public static void ScheduleReport(ReducerContext ctx, string expression, string timezone, uint maxFailures, string label, uint batchSize) =>
        CronScheduleJob(ctx, new CronJobArgsValue.report(new ReportCronArgs { Label = label, BatchSize = batchSize }),
            CronScheduleSpec.Cron(expression), new CronScheduleOpts { Timezone = timezone, MaxFailures = maxFailures });

    [SpacetimeDB.Reducer]
    public static void ScheduleProbe(ReducerContext ctx, string expression, string timezone, uint maxFailures, string source) =>
        CronScheduleJob(ctx, new CronJobArgsValue.probe(new ProbeCronArgs { Source = source }),
            CronScheduleSpec.Cron(expression), new CronScheduleOpts { Timezone = timezone, MaxFailures = maxFailures });

    [SpacetimeDB.Reducer]
    public static void UnscheduleJob(ReducerContext ctx, string name) => CronUnscheduleJob(ctx, name);

    [SpacetimeDB.Reducer]
    public static void SetFlakyFailing(ReducerContext ctx, bool failing)
    {
        if (ctx.Db.FlakyState.Singleton.Find(true) is { } s) ctx.Db.FlakyState.Singleton.Update(s with { Failing = failing });
    }

    /// <summary>Fault injection for the lost-fire reconciler.</summary>
    [SpacetimeDB.Reducer]
    public static void DropHeartbeatFireForTest(ReducerContext ctx) => ctx.Db.CronFire.JobName.Delete("heartbeat");

    /// <summary>Authorization probe: a direct call must not run a scheduler-only reducer.</summary>
    [SpacetimeDB.Reducer]
    public static void ForgeReportFireForTest(ReducerContext ctx)
    {
        var fire = ctx.Db.CronFire.JobName.Find("report") ?? throw new Exception("test.missing_report_fire");
        CronTick(ctx, fire);
    }

    // Parser and time-zone probes for the shared test vectors.

    [SpacetimeDB.Procedure]
    public static Result<long?, string> CronNextFireAfter(ProcedureContext ctx, string expression, long afterMicros, string timezone)
    {
        try { return Result<long?, string>.Ok(CronNextFireAfter(CronParseExpression(expression), afterMicros, timezone)); }
        catch (Exception e) { return Result<long?, string>.Err(e.Message); }
    }

    [SpacetimeDB.Procedure]
    public static bool CronIsValidTimezone(ProcedureContext ctx, string timezone) => CronIsValidTimezone(timezone);

    [SpacetimeDB.Procedure]
    public static int? CronUtcOffset(ProcedureContext ctx, string timezone, long atMicros) => CronUtcOffsetSeconds(timezone, atMicros);

    [SpacetimeDB.Procedure]
    public static List<long?> CronNextBatch(ProcedureContext ctx, List<CronCase> cases) => cases.Select(c =>
    {
        try { return CronNextFireAfter(CronParseExpression(c.Expression), c.AfterMicros, c.Timezone); }
        catch (Exception) { return null; }
    }).ToList();

    [SpacetimeDB.Procedure]
    public static List<bool> CronParseBatch(ProcedureContext ctx, List<string> expressions) => expressions.Select(e =>
    {
        try { CronParseExpression(e); return true; }
        catch (Exception) { return false; }
    }).ToList();

    [SpacetimeDB.Procedure]
    public static List<bool> CronValidTimezoneBatch(ProcedureContext ctx, List<string> timezones) => timezones.Select(CronIsValidTimezone).ToList();

    [SpacetimeDB.Procedure]
    public static List<int?> CronUtcOffsetBatch(ProcedureContext ctx, List<TzCase> cases) =>
        cases.Select(c => CronUtcOffsetSeconds(c.Timezone, c.UnixSeconds * 1_000_000)).ToList();
}
