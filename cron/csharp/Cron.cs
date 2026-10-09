// Cron.cs: calendar and interval jobs for C# modules.
//
// Unofficial C# port of @spacetimedb/cron (SpacetimeDB PR #5903, commit
// ae25f1da24b1723e28acd0038e75e598f33952c3, package spacetime-cron-ts, which uses
// cron-parser 5.5.0). Licensed Apache-2.0, like the original. Original design and behaviour
// by Clockwork Labs; this port is by Tinker (Gazz-Stripbolt).
//
// Declare the jobs' arguments, configure, and implement the handlers in your module:
//
//     [SpacetimeDB.Type]
//     public partial record CronJobArgsValue : TaggedEnum<(Unit heartbeat, ReportArgs report, Unit sync_feed)>;
//
//     public static partial class Module
//     {
//         public static partial CronSettings CronConfigure() => new(
//             Jobs: [CronJobSpec.Reducer("heartbeat"), CronJobSpec.Reducer("report"), CronJobSpec.Procedure("sync_feed")],
//             HistoryCap: 5, ReconcileEverySeconds: 0);
//
//         // Reducer jobs: inside the trigger's transaction. Throw to fail (writes roll back).
//         public static partial void CronRunJob(ReducerContext ctx, CronJobArgsValue job, CronInvocation inv) { ... }
//         // Procedure jobs: outside a transaction (use ctx.WithTx). Throw to fail.
//         public static partial void CronRunProcedureJob(ProcedureContext ctx, CronJobArgsValue job, CronInvocation inv) { }
//
//         [Reducer(ReducerKind.Init)]
//         public static void Init(ReducerContext ctx) =>
//             CronScheduleJob(ctx, new CronJobArgsValue.heartbeat(default), CronScheduleSpec.Every(30));
//     }
//
// Each job is a variant of CronJobArgsValue, and the variant's name is the job name.
//
// Same cron_job and cron_run tables, invocation ids (job:generation:sequence), failure counting
// and auto-disable, generations, catch-up, checkpointing and lost-fire repair as the TS package,
// and the same expression syntax and DST behaviour (cron-parser 5.5.0). Changes from the
// TypeScript original are the same as in cron.rs:
// - Triggers live in two shared tables, cron_fire (reducer jobs, scheduled reducer cron_tick)
//   and cron_procedure_fire (procedure jobs, scheduled procedure cron_tick_procedure), not a
//   <job>_fire table and <job>_cron function per job.
// - Time zones come from a table embedded below, generated from IANA tzdata (TzdataVersion).
//   UTC and ±HH:MM offsets work too; zone names are case-insensitive.
// - publicTables isn't supported (tables are private); expose CronPublicJobs in a view.
// - cron_job has an extra index on enabled, because views can only read indexes.

#pragma warning disable STDB_UNSTABLE
#nullable enable

using SpacetimeDB;

public enum CronJobKind
{
    /// <summary>Inside the trigger's transaction (CronRunJob).</summary>
    Reducer,
    /// <summary>As a procedure, outside a transaction (CronRunProcedureJob).</summary>
    Procedure,
}

/// <summary>A job's name (the CronJobArgsValue variant) and how it runs.</summary>
public readonly record struct CronJobSpec(string Name, CronJobKind Kind)
{
    public static CronJobSpec Reducer(string name) => new(name, CronJobKind.Reducer);
    public static CronJobSpec Procedure(string name) => new(name, CronJobKind.Procedure);
}

/// <summary>Module-wide settings, returned by your CronConfigure.</summary>
public sealed record CronSettings(CronJobSpec[] Jobs, uint HistoryCap = 5, uint ReconcileEverySeconds = 0);

/// <summary>Stable metadata passed to every invocation.</summary>
public readonly record struct CronInvocation(string Id, string JobName, ulong Generation, ulong Sequence, Timestamp ScheduledFor);

/// <summary>A cron expression or a fixed interval.</summary>
public readonly record struct CronScheduleSpec(string? Expression, uint EverySeconds)
{
    public static CronScheduleSpec Every(uint seconds) => new(null, seconds);
    public static CronScheduleSpec Cron(string expression) => new(expression, 0);
}

public sealed class CronScheduleOpts
{
    /// <summary>IANA time zone for cron expressions. Defaults to UTC.</summary>
    public string? Timezone { get; init; }
    /// <summary>Consecutive failures before the job disables itself. null/0 = never.</summary>
    public uint? MaxFailures { get; init; }
}

[SpacetimeDB.Type]
public partial struct CronSpec
{
    public string Expression;
    public string Timezone;
}

[SpacetimeDB.Type]
public partial struct EverySpec
{
    public uint Seconds;
}

[SpacetimeDB.Type]
public partial record CronSchedule : TaggedEnum<(CronSpec cron, EverySpec every)>;

[SpacetimeDB.Type]
public enum CronRunStatus
{
    Ok,
    Failed,
}

[SpacetimeDB.Type]
public partial struct CronFireRecovery
{
    public ulong Sequence;
    public Timestamp ScheduledFor;
    public string Error;
}

/// <summary>Public projection of cron_job, without arguments (see CronPublicJobs).</summary>
[SpacetimeDB.Type]
public partial struct CronJobView
{
    public string Name;
    public CronSchedule Schedule;
    public bool Enabled;
    public uint MaxFailures;
    public uint ConsecutiveFailures;
    public ulong FireCount;
    public ulong Generation;
    public Timestamp? LastRunAt;
    public Timestamp? NextRunAt;
    public string? DisabledReason;
}

public static partial class Module
{
    /// <summary>Codes a caller can receive (some followed by ':' and detail text).</summary>
    public static class CronErrors
    {
        public const string NotAuthorized = "cron.not_authorized";
        public const string InvalidMaxFailures = "cron.invalid_max_failures";
        public const string InvalidInterval = "cron.invalid_interval";
        public const string InvalidExpression = "cron.invalid_expression";
        public const string InvalidTimezone = "cron.invalid_timezone";
        public const string UnsatisfiableExpression = "cron.unsatisfiable_expression";
        public const string InvalidJobName = "cron.invalid_job_name";
        public const string InvalidScheduleState = "cron.invalid_schedule_state";
        public const string InvalidArgsState = "cron.invalid_args_state";
        public const string InvalidTrigger = "cron.invalid_trigger";
        public const string NoFutureOccurrence = "cron.no_future_occurrence";
        public const string InvalidProcedureRecovery = "cron.invalid_procedure_recovery";
    }

    /// <summary>Implement: the jobs and settings.</summary>
    public static partial CronSettings CronConfigure();
    /// <summary>Implement: run a reducer job. Throw to fail (its writes roll back).</summary>
    public static partial void CronRunJob(ReducerContext ctx, CronJobArgsValue job, CronInvocation inv);
    /// <summary>Implement: run a procedure job (outside a transaction). Throw to fail.</summary>
    public static partial void CronRunProcedureJob(ProcedureContext ctx, CronJobArgsValue job, CronInvocation inv);

    const long CronOneSecondMicros = 1_000_000;
    public const uint CronMaxIntervalSeconds = 31_536_000;
    public const uint CronMaxHistoryCap = 1_000;
    public const int CronMaxErrorLength = 1_024;
    public const int CronMaxExpressionLength = 256;
    public const int CronMaxTimezoneLength = 128;
    const int CronMaxJobNameLength = 48;
    const long CronCheckpointDelayMicros = 365L * 24 * 60 * 60 * CronOneSecondMicros;
    const string CronReconcileTickKey = "cron";
    const long CronMaxSafeDateMs = 8_640_000_000_000_000;

    static CronSettings? cronSettings;
    static CronSettings CronSetup => cronSettings ??= CronConfigure();

    // -----------------------------------------------------------------------
    // Tables
    // -----------------------------------------------------------------------

    [SpacetimeDB.Table(Accessor = "CronJob")]
    public partial struct CronJob
    {
        [SpacetimeDB.PrimaryKey]
        public string Name;
        public CronSchedule Schedule;
        public CronJobArgsValue Args;
        [SpacetimeDB.Index.BTree]
        public bool Enabled;
        public uint MaxFailures;
        public uint ConsecutiveFailures;
        public ulong FireCount;
        public ulong Generation;
        public Timestamp? LastRunAt;
        public Timestamp? NextRunAt;
        public string? DisabledReason;
    }

    [SpacetimeDB.Table(Accessor = "CronRun")]
    public partial struct CronRun
    {
        [SpacetimeDB.PrimaryKey]
        public string InvocationId;
        [SpacetimeDB.Index.BTree]
        public string JobName;
        public ulong Generation;
        public ulong Sequence;
        public Timestamp ScheduledFor;
        public Timestamp CompletedAt;
        public CronRunStatus Status;
        public string? Error;
    }

    /// <summary>Pending triggers of reducer jobs (one per job).</summary>
    [SpacetimeDB.Table(Accessor = "CronFire", Scheduled = nameof(CronTick), ScheduledAt = nameof(CronFire.ScheduledAt))]
    public partial struct CronFire
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
        [SpacetimeDB.Unique]
        public string JobName;
        public ulong Generation;
        public Timestamp? TargetAt;
        public CronFireRecovery? Recovery;
    }

    /// <summary>Pending triggers of procedure jobs (one per job). Same columns as cron_fire.</summary>
    [SpacetimeDB.Table(Accessor = "CronProcedureFire", Scheduled = nameof(CronTickProcedure), ScheduledAt = nameof(CronProcedureFire.ScheduledAt))]
    public partial struct CronProcedureFire
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
        [SpacetimeDB.Unique]
        public string JobName;
        public ulong Generation;
        public Timestamp? TargetAt;
        public CronFireRecovery? Recovery;
    }

    [SpacetimeDB.Table(Accessor = "CronReconcileTick", Scheduled = nameof(CronReconcile), ScheduledAt = nameof(CronReconcileTick.ScheduledAt))]
    public partial struct CronReconcileTick
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
        [SpacetimeDB.Unique]
        public string Key;
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// <summary>The job a CronJobArgsValue belongs to (its variant name).</summary>
    public static string CronJobNameOf(CronJobArgsValue args) => args.GetType().Name;

    /// <summary>
    /// Create or replace a job's schedule and arm its first trigger. The job is the variant of
    /// args. A new generation starts: pending triggers from the old schedule are dropped.
    /// </summary>
    public static void CronScheduleJob(ReducerContext ctx, CronJobArgsValue args, CronScheduleSpec spec, CronScheduleOpts? opts = null)
    {
        var jobName = CronJobNameOf(args);
        var kind = CronJobKindOf(jobName);
        var now = ctx.Timestamp.MicrosecondsSinceUnixEpoch;
        var (schedule, firstAt) = CronNormalizeSchedule(spec, opts?.Timezone, now);
        var maxFailures = opts?.MaxFailures ?? 0;

        CronEnsureReconcileTick(ctx);
        CronReconcileLostFires(ctx);

        var existing = ctx.Db.CronJob.Name.Find(jobName);
        var generation = (existing?.Generation ?? 0) + 1;
        CronDisarmFire(ctx, kind, jobName);
        var row = new CronJob
        {
            Name = jobName,
            Schedule = schedule,
            Args = args,
            Enabled = true,
            MaxFailures = maxFailures,
            ConsecutiveFailures = 0,
            FireCount = existing?.FireCount ?? 0,
            Generation = generation,
            LastRunAt = existing?.LastRunAt,
            NextRunAt = null,
            DisabledReason = null,
        };
        if (existing is not null) ctx.Db.CronJob.Name.Update(row);
        else ctx.Db.CronJob.Insert(row);
        var next = CronInsertFire(ctx, kind, row, firstAt) ?? throw new Exception(CronErrors.UnsatisfiableExpression);
        ctx.Db.CronJob.Name.Update(row with { NextRunAt = next });
    }

    /// <summary>Disable a job and remove its pending trigger.</summary>
    public static void CronUnscheduleJob(ReducerContext ctx, string jobName)
    {
        var kind = CronJobKindOf(jobName);
        CronEnsureReconcileTick(ctx);
        CronReconcileLostFires(ctx);
        if (ctx.Db.CronJob.Name.Find(jobName) is not { } row) return;
        CronDisarmFire(ctx, kind, jobName);
        ctx.Db.CronJob.Name.Update(row with
        {
            Enabled = false,
            Generation = row.Generation + 1,
            ConsecutiveFailures = 0,
            NextRunAt = null,
            DisabledReason = "disabled_by_operator",
        });
    }

    /// <summary>
    /// Sanitized job state for a public view (no arguments; internal error text mapped to a short
    /// reason). Wrap it in your own [View(Accessor = "CronJobs", Public = true)].
    /// </summary>
    public static List<CronJobView> CronPublicJobs(ViewContext ctx) =>
        ctx.Db.CronJob.Enabled.Filter(true).Concat(ctx.Db.CronJob.Enabled.Filter(false))
            .Select(r => new CronJobView
            {
                Name = r.Name,
                Schedule = r.Schedule,
                Enabled = r.Enabled,
                MaxFailures = r.MaxFailures,
                ConsecutiveFailures = r.ConsecutiveFailures,
                FireCount = r.FireCount,
                Generation = r.Generation,
                LastRunAt = r.LastRunAt,
                NextRunAt = r.NextRunAt,
                DisabledReason = CronPublicDisabledReason(r.DisabledReason),
            })
            .ToList();

    /// <summary>Map a stored disabled reason to the public vocabulary.</summary>
    public static string? CronPublicDisabledReason(string? reason)
    {
        if (reason is null) return null;
        if (reason == "disabled_by_operator") return "disabled_by_operator";
        if (reason.StartsWith(CronErrors.InvalidScheduleState + ":", StringComparison.Ordinal)) return "invalid_schedule_state";
        if (CronConsecutiveFailureReason(reason) is { } rest) return rest == "lost_fire" ? "lost_fire_threshold_reached" : "failure_threshold_reached";
        return "disabled";
    }

    static string? CronConsecutiveFailureReason(string r)
    {
        if (!r.StartsWith("failed_", StringComparison.Ordinal)) return null;
        var i = "failed_".Length;
        var start = i;
        while (i < r.Length && char.IsAsciiDigit(r[i])) i++;
        if (i == start || r[start] == '0') return null;
        const string mid = "_consecutive_times:";
        return string.CompareOrdinal(r, i, mid, 0, mid.Length) == 0 && r.Length >= i + mid.Length ? r[(i + mid.Length)..] : null;
    }

    // -----------------------------------------------------------------------
    // Scheduled functions
    // -----------------------------------------------------------------------

    /// <summary>Fires reducer jobs.</summary>
    [SpacetimeDB.Reducer]
    public static void CronTick(ReducerContext ctx, CronFire arg)
    {
        CronRequireDatabaseCaller(ctx);
        if (CronJobSpecOf(arg.JobName) is not { Kind: CronJobKind.Reducer }) return;
        var fire = CronFireRow.From(arg);
        if (arg.Recovery is not null)
        {
            CronRecoverFailure(ctx, fire);
            return;
        }
        if (CronPrepareFire(ctx, CronJobKind.Reducer, fire, false) is not var (args, invocation)) return;
        try
        {
            CronRunJob(ctx, args, invocation);
        }
        catch (Exception e)
        {
            // Best effort: record the failure in a new transaction once this one rolls back.
            // Reconciliation repairs a lost calendar trigger if the host drops this call.
            var recovery = arg with
            {
                Recovery = new CronFireRecovery
                {
                    Sequence = invocation.Sequence,
                    ScheduledFor = invocation.ScheduledFor,
                    Error = CronTruncateError(e.Message),
                },
            };
            CronScheduleRecovery(recovery);
            throw;
        }
        CronApplyOutcome(ctx, CronJobKind.Reducer, invocation, CronRunStatus.Ok, null, true);
    }

    /// <summary>Volatile, non-atomic: survives this transaction's rollback.</summary>
    static void CronScheduleRecovery(CronFire recovery)
    {
        using var stream = new MemoryStream();
        using var writer = new BinaryWriter(stream);
        new CronFire.BSATN().Write(writer, recovery);
        // The canonical reducer name: the generated helper passes the C# method name.
        SpacetimeDB.Internal.IReducer.VolatileNonatomicScheduleImmediate("cron_tick", stream);
    }

    /// <summary>Fires procedure jobs: reserve the sequence, run outside a transaction, record the outcome.</summary>
    [SpacetimeDB.Procedure]
    public static void CronTickProcedure(ProcedureContext ctx, CronProcedureFire arg)
    {
        if (ctx.Sender != ProcedureContextBase.Identity) throw new Exception(CronErrors.NotAuthorized);
        if (arg.Recovery is not null) throw new Exception(CronErrors.InvalidProcedureRecovery);
        var fire = CronFireRow.From(arg);
        var prepared = ctx.WithTx(tx => CronPrepareFire(tx, CronJobKind.Procedure, fire, true));
        if (prepared is not var (args, invocation)) return;
        string? error = null;
        try
        {
            CronRunProcedureJob(ctx, args, invocation);
        }
        catch (Exception e)
        {
            error = CronTruncateError(e.Message);
        }
        ctx.WithTx(tx =>
        {
            CronApplyOutcome(tx, CronJobKind.Procedure, invocation, error is null ? CronRunStatus.Ok : CronRunStatus.Failed, error, false);
            return 0;
        });
    }

    /// <summary>Optional periodic repair of enabled jobs whose trigger went missing.</summary>
    [SpacetimeDB.Reducer]
    public static void CronReconcile(ReducerContext ctx, CronReconcileTick tick)
    {
        CronRequireDatabaseCaller(ctx);
        CronReconcileLostFires(ctx);
    }

    // -----------------------------------------------------------------------
    // Engine (a port of cron.ts)
    // -----------------------------------------------------------------------

    static CronJobSpec? CronJobSpecOf(string name)
    {
        foreach (var j in CronSetup.Jobs) if (j.Name == name) return j;
        return null;
    }

    static CronJobKind CronJobKindOf(string name)
    {
        CronValidateJobName(name);
        return CronJobSpecOf(name)?.Kind ?? throw new Exception($"cron.unknown_job:{name}");
    }

    static void CronValidateJobName(string name)
    {
        // /^[a-z][a-z0-9]*(?:_[a-z0-9]+)*$/, at most 48 characters.
        var ok = name.Length > 0 && name.Length <= CronMaxJobNameLength && name[0] is >= 'a' and <= 'z'
            && name.Split('_').All(p => p.Length > 0 && p.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9'));
        if (!ok) throw new Exception($"{CronErrors.InvalidJobName}:use 1-48 lowercase snake_case characters");
    }

    static void CronRequireDatabaseCaller(ReducerContext ctx)
    {
        if (ctx.Sender != ctx.DatabaseIdentity) throw new Exception(CronErrors.NotAuthorized);
    }

    static string CronInvocationId(string job, ulong generation, ulong sequence) => $"{job}:{generation}:{sequence}";

    public static string CronTruncateError(string message) =>
        message.Length <= CronMaxErrorLength ? message : message[..(CronMaxErrorLength - 3)] + "...";

    /// <summary>The columns both fire tables share.</summary>
    readonly record struct CronFireRow(ulong ScheduledId, string JobName, ulong Generation, Timestamp? TargetAt, CronFireRecovery? Recovery)
    {
        public static CronFireRow From(CronFire f) => new(f.ScheduledId, f.JobName, f.Generation, f.TargetAt, f.Recovery);
        public static CronFireRow From(CronProcedureFire f) => new(f.ScheduledId, f.JobName, f.Generation, f.TargetAt, f.Recovery);
    }

    static CronFireRow? CronFindFire(Local db, CronJobKind kind, string job) => kind == CronJobKind.Reducer
        ? db.CronFire.JobName.Find(job) is { } f ? CronFireRow.From(f) : null
        : db.CronProcedureFire.JobName.Find(job) is { } p ? CronFireRow.From(p) : null;

    static void CronDeleteFire(Local db, CronJobKind kind, ulong scheduledId)
    {
        if (kind == CronJobKind.Reducer) db.CronFire.ScheduledId.Delete(scheduledId);
        else db.CronProcedureFire.ScheduledId.Delete(scheduledId);
    }

    static void CronDisarmFire(ReducerContext ctx, CronJobKind kind, string job) => CronDisarmFire(ctx.Db, kind, job);

    static void CronDisarmFire(Local db, CronJobKind kind, string job)
    {
        if (CronFindFire(db, kind, job) is { } f) CronDeleteFire(db, kind, f.ScheduledId);
    }

    static void CronInsertFireRow(Local db, CronJobKind kind, ScheduleAt at, string job, ulong generation, Timestamp? targetAt)
    {
        if (kind == CronJobKind.Reducer)
            db.CronFire.Insert(new CronFire { ScheduledId = 0, ScheduledAt = at, JobName = job, Generation = generation, TargetAt = targetAt, Recovery = null });
        else
            db.CronProcedureFire.Insert(new CronProcedureFire { ScheduledId = 0, ScheduledAt = at, JobName = job, Generation = generation, TargetAt = targetAt, Recovery = null });
    }

    static Timestamp CronTs(long micros) => new(micros);

    static long CronBoundedScheduleTime(long now, long target) => Math.Min(target, now + CronCheckpointDelayMicros);

    static Timestamp? CronInsertFire(ReducerContext ctx, CronJobKind kind, CronJob row, long? targetMicros) =>
        CronInsertFire(ctx.Db, ctx.Timestamp, kind, row, targetMicros);

    /// <summary>
    /// Arm the next trigger: interval jobs get a repeating interval schedule, calendar jobs a
    /// one-shot (checkpointed at most a year out). Returns the next run time.
    /// </summary>
    static Timestamp? CronInsertFire(Local db, Timestamp nowTs, CronJobKind kind, CronJob row, long? targetMicros)
    {
        var now = nowTs.MicrosecondsSinceUnixEpoch;
        switch (row.Schedule)
        {
            case CronSchedule.every(var e):
                var every = e.Seconds * CronOneSecondMicros;
                CronInsertFireRow(db, kind, new ScheduleAt.Interval(new TimeDuration(every)), row.Name, row.Generation, null);
                return CronTs(now + every);
            default:
                if (targetMicros is not { } target) return null;
                CronInsertFireRow(db, kind, new ScheduleAt.Time(CronTs(CronBoundedScheduleTime(now, target))), row.Name, row.Generation, CronTs(target));
                return CronTs(target);
        }
    }

    /// <summary>Next occurrence; throws on an unparsable stored expression.</summary>
    static long? CronNextOccurrence(CronSchedule schedule, long afterMicros) => schedule switch
    {
        CronSchedule.every(var e) => afterMicros + e.Seconds * CronOneSecondMicros,
        CronSchedule.cron(var c) => CronNextFireAfter(CronParseExpression(c.Expression), afterMicros, c.Timezone),
        _ => null,
    };

    static CronJob CronReplaceFire(Local db, Timestamp now, CronJobKind kind, CronJob row)
    {
        CronDisarmFire(db, kind, row.Name);
        long? target = null;
        if (row.Schedule is CronSchedule.cron)
        {
            try
            {
                target = CronNextOccurrence(row.Schedule, now.MicrosecondsSinceUnixEpoch);
            }
            catch (Exception e)
            {
                return CronDisableJob(db, kind, row, $"{CronErrors.InvalidScheduleState}:{CronTruncateError(e.Message)}");
            }
            if (target is null) return CronDisableJob(db, kind, row, CronErrors.NoFutureOccurrence);
        }
        var updated = row with { NextRunAt = CronInsertFire(db, now, kind, row, target) };
        db.CronJob.Name.Update(updated);
        return updated;
    }

    static void CronEnsureReconcileTick(ReducerContext ctx)
    {
        var every = CronSetup.ReconcileEverySeconds;
        if (every == 0 || ctx.Db.CronReconcileTick.Key.Find(CronReconcileTickKey) is not null) return;
        ctx.Db.CronReconcileTick.Insert(new CronReconcileTick
        {
            ScheduledId = 0,
            ScheduledAt = new ScheduleAt.Interval(new TimeDuration(every * CronOneSecondMicros)),
            Key = CronReconcileTickKey,
        });
    }

    static void CronReconcileLostFires(ReducerContext ctx)
    {
        foreach (var (name, kind) in CronSetup.Jobs)
        {
            if (ctx.Db.CronJob.Name.Find(name) is not { Enabled: true } row)
            {
                CronDisarmFire(ctx, kind, name);
                continue;
            }
            if (CronJobNameOf(row.Args) != name)
            {
                CronDisableJob(ctx.Db, kind, row, CronErrors.InvalidArgsState);
                continue;
            }
            var pending = CronFindFire(ctx.Db, kind, name);
            var isCron = row.Schedule is CronSchedule.cron;
            if (pending is { } p && p.Generation == row.Generation && p.Recovery is null && p.TargetAt.HasValue == isCron) continue;
            var sequence = row.FireCount + 1;
            var invocation = new CronInvocation(CronInvocationId(name, row.Generation, sequence), name, row.Generation, sequence, row.NextRunAt ?? ctx.Timestamp);
            CronReplaceFire(ctx.Db, ctx.Timestamp, kind, row);
            CronApplyOutcome(ctx.Db, ctx.Timestamp, kind, invocation, CronRunStatus.Failed, "lost_fire", true);
        }
    }

    static CronJob CronDisableJob(Local db, CronJobKind kind, CronJob row, string reason)
    {
        CronDisarmFire(db, kind, row.Name);
        var updated = row with { Enabled = false, NextRunAt = null, DisabledReason = CronTruncateError(reason) };
        db.CronJob.Name.Update(updated);
        return updated;
    }

    static void CronApplyOutcome(ReducerContext ctx, CronJobKind kind, CronInvocation inv, CronRunStatus status, string? error, bool advance) =>
        CronApplyOutcome(ctx.Db, ctx.Timestamp, kind, inv, status, error, advance);

    static void CronApplyOutcome(ProcedureTxContext tx, CronJobKind kind, CronInvocation inv, CronRunStatus status, string? error, bool advance) =>
        CronApplyOutcome(tx.Db, tx.Timestamp, kind, inv, status, error, advance);

    static void CronApplyOutcome(Local db, Timestamp now, CronJobKind kind, CronInvocation invocation, CronRunStatus status, string? error, bool advanceFireCount)
    {
        if (db.CronRun.InvocationId.Find(invocation.Id) is not null) return;
        if (db.CronJob.Name.Find(invocation.JobName) is { } row && row.Generation == invocation.Generation)
        {
            if (advanceFireCount && invocation.Sequence != row.FireCount + 1) return;
            var consecutive = status == CronRunStatus.Failed ? (row.ConsecutiveFailures == uint.MaxValue ? uint.MaxValue : row.ConsecutiveFailures + 1) : 0;
            row = row with
            {
                FireCount = Math.Max(invocation.Sequence, row.FireCount),
                ConsecutiveFailures = consecutive,
                LastRunAt = now,
            };
            db.CronJob.Name.Update(row);
            if (status == CronRunStatus.Failed && row.Enabled && row.MaxFailures > 0 && consecutive >= row.MaxFailures)
            {
                CronDisableJob(db, kind, row, $"failed_{consecutive}_consecutive_times:{error ?? "Failed"}");
            }
        }
        CronRecordRun(db, now, invocation, status, error);
    }

    static void CronRecordRun(Local db, Timestamp now, CronInvocation inv, CronRunStatus status, string? error)
    {
        var cap = (int)Math.Min(CronSetup.HistoryCap, CronMaxHistoryCap);
        if (cap == 0 || db.CronRun.InvocationId.Find(inv.Id) is not null) return;
        db.CronRun.Insert(new CronRun
        {
            InvocationId = inv.Id,
            JobName = inv.JobName,
            Generation = inv.Generation,
            Sequence = inv.Sequence,
            ScheduledFor = inv.ScheduledFor,
            CompletedAt = now,
            Status = status,
            Error = error,
        });
        var rows = db.CronRun.JobName.Filter(inv.JobName).OrderBy(r => r.Sequence).ToList();
        foreach (var r in rows.Take(Math.Max(0, rows.Count - cap))) db.CronRun.InvocationId.Delete(r.InvocationId);
    }

    static (CronJobArgsValue, CronInvocation)? CronPrepareFire(ReducerContext ctx, CronJobKind kind, CronFireRow arg, bool reserve) =>
        CronPrepareFire(ctx.Db, ctx.Timestamp, kind, arg, reserve);

    static (CronJobArgsValue, CronInvocation)? CronPrepareFire(ProcedureTxContext tx, CronJobKind kind, CronFireRow arg, bool reserve) =>
        CronPrepareFire(tx.Db, tx.Timestamp, kind, arg, reserve);

    /// <summary>
    /// Work out the invocation for a trigger, re-arm the next one, and return the job's
    /// arguments. null when the trigger is stale or the job can't run.
    /// </summary>
    static (CronJobArgsValue, CronInvocation)? CronPrepareFire(Local db, Timestamp nowTs, CronJobKind kind, CronFireRow arg, bool reserveSequence)
    {
        if (db.CronJob.Name.Find(arg.JobName) is not { } row) return null;
        if (!row.Enabled || row.Generation != arg.Generation) return null;
        if (CronJobNameOf(row.Args) != arg.JobName)
        {
            CronDisableJob(db, kind, row, CronErrors.InvalidArgsState);
            return null;
        }
        var now = nowTs.MicrosecondsSinceUnixEpoch;
        var scheduledFor = nowTs;
        if (row.Schedule is CronSchedule.cron)
        {
            if (arg.TargetAt is not { } targetAt)
            {
                CronDisableJob(db, kind, row, $"{CronErrors.InvalidTrigger}:missing_target");
                return null;
            }
            CronDeleteFire(db, kind, arg.ScheduledId);
            var target = targetAt.MicrosecondsSinceUnixEpoch;
            if (target > now)
            {
                // A yearly checkpoint woke us before the occurrence: re-arm for it.
                db.CronJob.Name.Update(row with { NextRunAt = CronInsertFire(db, nowTs, kind, row, target) });
                return null;
            }
            scheduledFor = targetAt;
            long? next;
            try { next = CronNextOccurrence(row.Schedule, now); }
            catch (Exception) { next = null; }
            if (next is { } n)
            {
                row = row with { NextRunAt = CronInsertFire(db, nowTs, kind, row, n) };
                db.CronJob.Name.Update(row);
            }
            else row = CronDisableJob(db, kind, row, CronErrors.NoFutureOccurrence);
        }
        else if (row.Schedule is CronSchedule.every(var e))
        {
            row = row with { NextRunAt = CronTs(now + e.Seconds * CronOneSecondMicros) };
            db.CronJob.Name.Update(row);
        }
        var sequence = row.FireCount + 1;
        if (reserveSequence)
        {
            row = row with { FireCount = sequence };
            db.CronJob.Name.Update(row);
        }
        return (row.Args, new CronInvocation(CronInvocationId(row.Name, row.Generation, sequence), row.Name, row.Generation, sequence, scheduledFor));
    }

    static void CronRecoverFailure(ReducerContext ctx, CronFireRow arg)
    {
        if (arg.Recovery is not { } recovery) return;
        const CronJobKind kind = CronJobKind.Reducer;
        if (ctx.Db.CronJob.Name.Find(arg.JobName) is not { } row) return;
        if (!row.Enabled || row.Generation != arg.Generation || recovery.Sequence != row.FireCount + 1) return;
        if (row.Schedule is CronSchedule.every(var e))
        {
            ctx.Db.CronJob.Name.Update(row with { NextRunAt = CronTs(ctx.Timestamp.MicrosecondsSinceUnixEpoch + e.Seconds * CronOneSecondMicros) });
        }
        else
        {
            // Replace the trigger unconditionally so stale state can't block recovery.
            CronReplaceFire(ctx.Db, ctx.Timestamp, kind, row);
        }
        var invocation = new CronInvocation(CronInvocationId(arg.JobName, arg.Generation, recovery.Sequence), arg.JobName, arg.Generation, recovery.Sequence, recovery.ScheduledFor);
        CronApplyOutcome(ctx, kind, invocation, CronRunStatus.Failed, CronTruncateError(recovery.Error), true);
    }

    static (CronSchedule, long?) CronNormalizeSchedule(CronScheduleSpec spec, string? timezone, long now)
    {
        if (spec.Expression is null)
        {
            var seconds = spec.EverySeconds;
            if (seconds < 1 || seconds > CronMaxIntervalSeconds)
                throw new Exception($"{CronErrors.InvalidInterval}:seconds must be an integer between 1 and {CronMaxIntervalSeconds}");
            return (new CronSchedule.every(new EverySpec { Seconds = seconds }), now + seconds * CronOneSecondMicros);
        }
        var expression = spec.Expression.Trim();
        var tz = (timezone ?? "UTC").Trim();
        if (tz.Length == 0) tz = "UTC";
        if (expression.Length == 0) throw new Exception($"{CronErrors.InvalidExpression}:empty");
        if (expression.Length > CronMaxExpressionLength) throw new Exception($"{CronErrors.InvalidExpression}:too_long");
        if (tz.Length > CronMaxTimezoneLength) throw new Exception($"{CronErrors.InvalidTimezone}:too_long");
        if (!CronIsValidTimezone(tz)) throw new Exception($"{CronErrors.InvalidTimezone}:{tz}");
        CronParsed parsed;
        try { parsed = CronParseExpression(expression); }
        catch (Exception e) { throw new Exception($"{CronErrors.InvalidExpression}:{e.Message}"); }
        var first = CronNextFireAfter(parsed, now, tz) ?? throw new Exception(CronErrors.UnsatisfiableExpression);
        return (new CronSchedule.cron(new CronSpec { Expression = expression, Timezone = tz }), first);
    }

    // -----------------------------------------------------------------------
    // Cron expressions (a port of cron-parser 5.5.0's parsing and search)
    // -----------------------------------------------------------------------

    /// <summary>The hashSeed the TS package passes to cron-parser, so H resolves identically.</summary>
    const string CronHashSeed = "spacetimedb-cron-submodule";

    /// <summary>A parsed cron expression.</summary>
    public sealed class CronParsed
    {
        internal readonly bool[] Seconds = new bool[60], Minutes = new bool[60], Hours = new bool[24], Months = new bool[13], Dom = new bool[32];
        internal readonly bool[] Dow = new bool[7], DowLast = new bool[7];
        internal bool DomLast, DomWildcard, DowHasLast, DowLastInvalid, DowWildcard;
        internal int Nth;
    }

    /// <summary>cron-parser's seeded PRNG: FNV-1a of the seed into mulberry32.</summary>
    sealed class CronMulberry32(string seed)
    {
        uint state = Fnv(seed);

        static uint Fnv(string s)
        {
            var h = 2166136261u;
            foreach (var c in s)
            {
                h ^= c;
                h *= 16777619;
            }
            return h;
        }

        public double Next()
        {
            unchecked
            {
                state += 0x6d2b79f5;
                var t = state;
                t = (t ^ (t >> 15)) * (t | 1);
                t ^= t + (t ^ (t >> 7)) * (t | 61);
                return (t ^ (t >> 14)) / 4294967296.0;
            }
        }
    }

    enum CronUnit { Second, Minute, Hour, DayOfMonth, Month, DayOfWeek }

    static (long Min, long Max) CronBounds(CronUnit u) => u switch
    {
        CronUnit.Second or CronUnit.Minute => (0, 59),
        CronUnit.Hour => (0, 23),
        CronUnit.DayOfMonth => (1, 31),
        CronUnit.Month => (1, 12),
        _ => (0, 7),
    };

    static bool CronAllowsL(CronUnit u) => u is CronUnit.DayOfMonth or CronUnit.DayOfWeek;

    /// <summary>A parsed field value: a number, or an L form ("L", "5L").</summary>
    readonly record struct CronValue(long Num, string? Str);

    /// <summary>JS parseInt(s, 10).</summary>
    static long? CronJsParseInt(string s)
    {
        var t = s.TrimStart();
        var i = 0;
        var neg = false;
        if (t.Length > 0 && (t[0] == '-' || t[0] == '+')) { neg = t[0] == '-'; i = 1; }
        var start = i;
        while (i < t.Length && char.IsAsciiDigit(t[i])) i++;
        if (i == start) return null;
        var v = long.TryParse(t[start..i], out var n) ? n : long.MaxValue;
        return neg ? -v : v;
    }

    /// <summary>JS unary + for the strings that can reach it (digits, letters, L).</summary>
    static long? CronJsNumber(string s)
    {
        var t = s.Trim();
        if (t.Length == 0) return 0;
        return t.All(char.IsAsciiDigit) && long.TryParse(t, out var n) ? n : null;
    }

    static bool CronValidChars(CronUnit unit, string value)
    {
        var baseOk = value.Length > 0 && value.All(c => char.IsAsciiDigit(c) || c is '?' or ',' or '*' or 'H' or '/' or '-'
            || (c == 'L' && CronAllowsL(unit)) || (c == '#' && unit == CronUnit.DayOfWeek));
        return baseOk || CronHashedForms(value);
    }

    static bool CronHashedForms(string v)
    {
        for (var i = 0; i < v.Length; i++)
        {
            if (v[i] != 'H') continue;
            var j = i + 1;
            if (j < v.Length && v[j] == '(')
            {
                var k = j + 1;
                var d1 = k;
                while (k < v.Length && char.IsAsciiDigit(v[k])) k++;
                if (k > d1 && k < v.Length && v[k] == '-')
                {
                    var d2 = ++k;
                    while (k < v.Length && char.IsAsciiDigit(v[k])) k++;
                    if (k > d2 && k < v.Length && v[k] == ')') return true;
                }
            }
            else if (j < v.Length && v[j] == '/' && j + 1 < v.Length && char.IsAsciiDigit(v[j + 1])) return true;
        }
        return false;
    }

    static string CronReplaceAliases(string value)
    {
        string[] months = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
        string[] days = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
        var sb = new System.Text.StringBuilder();
        var i = 0;
        while (i < value.Length)
        {
            if (i + 3 <= value.Length && char.IsAsciiLetter(value[i]) && char.IsAsciiLetter(value[i + 1]) && char.IsAsciiLetter(value[i + 2]))
            {
                var word = value.Substring(i, 3).ToLowerInvariant();
                var m = Array.IndexOf(months, word);
                var d = Array.IndexOf(days, word);
                if (m >= 0) sb.Append(m + 1);
                else if (d >= 0) sb.Append(d);
                else throw new Exception($"Validation error, cannot resolve alias \"{word}\"");
                i += 3;
            }
            else sb.Append(value[i++]);
        }
        return sb.ToString();
    }

    static string CronReplaceHashed(string value, CronUnit unit, double random)
    {
        var (min, max) = CronBounds(unit);
        var sb = new System.Text.StringBuilder();
        var i = 0;
        (long? n, int len) ReadNum(int at)
        {
            var k = at;
            while (k < value.Length && char.IsAsciiDigit(value[k])) k++;
            return k == at ? (null, 0) : (long.Parse(value[at..k]), k - at);
        }
        while (i < value.Length)
        {
            if (value[i] != 'H')
            {
                sb.Append(value[i++]);
                continue;
            }
            var j = i + 1;
            (long A, long B)? range = null;
            if (j < value.Length && value[j] == '(')
            {
                var (a, la) = ReadNum(j + 1);
                if (a is { } av && j + 1 + la < value.Length && value[j + 1 + la] == '-')
                {
                    var k = j + 1 + la + 1;
                    var (b, lb) = ReadNum(k);
                    if (b is { } bv && k + lb < value.Length && value[k + lb] == ')')
                    {
                        range = (av, bv);
                        j = k + lb + 1;
                    }
                }
            }
            long? step = null;
            if (j < value.Length && value[j] == '/')
            {
                var (s, ls) = ReadNum(j + 1);
                if (s is { } sv)
                {
                    step = sv;
                    j += 1 + ls;
                }
            }
            string replacement;
            if (range is { } r && step is { } st)
            {
                if (r.A > r.B) throw new Exception($"Invalid range: {r.A}-{r.B}, min > max");
                if (st <= 0) throw new Exception($"Invalid step: {st}, must be positive");
                var minStart = Math.Max(r.A, min);
                var offset = (long)Math.Floor(random * st);
                var vals = new List<string>();
                for (var x = minStart / st * st + offset; x <= r.B; x += st) if (x >= minStart) vals.Add(x.ToString());
                replacement = string.Join(",", vals);
            }
            else if (range is { } r2)
            {
                if (r2.A > r2.B) throw new Exception($"Invalid range: {r2.A}-{r2.B}, min > max");
                replacement = ((long)Math.Floor(random * (r2.B - r2.A + 1)) + r2.A).ToString();
            }
            else if (step is { } st2)
            {
                if (st2 <= 0) throw new Exception($"Invalid step: {st2}, must be positive");
                var offset = (long)Math.Floor(random * st2);
                var vals = new List<string>();
                for (var x = min / st2 * st2 + offset; x <= max; x += st2) if (x >= min) vals.Add(x.ToString());
                replacement = string.Join(",", vals);
            }
            else replacement = ((long)Math.Floor(random * (max - min + 1) + min)).ToString();
            sb.Append(replacement);
            i = j;
        }
        return sb.ToString();
    }

    static List<CronValue> CronParseField(CronUnit unit, string raw, double random)
    {
        var value = raw;
        if (unit is CronUnit.Month or CronUnit.DayOfWeek) value = CronReplaceAliases(value);
        if (!CronValidChars(unit, value)) throw new Exception($"Invalid characters, got value: {value}");
        var (min, max) = CronBounds(unit);
        value = value.Replace("*", $"{min}-{max}").Replace("?", $"{min}-{max}");
        value = CronReplaceHashed(value, unit, random);

        var stack = new List<CronValue>();
        bool HasNum(long n) => stack.Any(v => v.Str is null && v.Num == n);
        foreach (var atom in value.Split(','))
        {
            if (atom.Length == 0) throw new Exception("Invalid list value format");
            var parts = atom.Split('/');
            if (parts.Length > 2) throw new Exception($"Invalid repeat: {atom}");
            string rangeSrc;
            long? repeat;
            if (parts.Length == 2)
            {
                rangeSrc = CronJsParseInt(parts[0]) is not null ? $"{parts[0]}-{max}" : parts[0];
                repeat = CronJsParseInt(parts[1]);
            }
            else
            {
                rangeSrc = atom;
                repeat = 1;
            }
            var pieces = rangeSrc.Split('-');
            if (pieces.Length <= 1)
            {
                var num = CronJsNumber(rangeSrc);
                if (num is null && CronAllowsL(unit) && rangeSrc.Contains('L'))
                {
                    stack.Add(new CronValue(0, rangeSrc));
                    continue;
                }
                var text = num is { } nv ? nv.ToString() : rangeSrc;
                if (CronJsParseInt(text) is { } n && n >= min && n <= max) stack.Add(new CronValue(unit == CronUnit.DayOfWeek ? n % 7 : n, null));
                else throw new Exception($"Constraint error, got value {text} expected range {min}-{max}");
                continue;
            }
            var a = CronJsParseInt(pieces[0]);
            var b = CronJsParseInt(pieces[1]);
            if (a is not { } av || b is not { } bv)
                throw new Exception($"Constraint error, got range {pieces[0]}-{pieces[1]} expected range {min}-{max}");
            if (av < min || bv > max) throw new Exception($"Constraint error, got range {av}-{bv} expected range {min}-{max}");
            if (av > bv) throw new Exception($"Invalid range: {av}-{bv}, min({av}) > max({bv})");
            if (repeat is not { } step || step <= 0)
                throw new Exception($"Constraint error, cannot repeat at every {(parts.Length > 1 ? parts[1] : "")} time.");
            if (unit == CronUnit.DayOfWeek && bv % 7 == 0 && !HasNum(0)) stack.Add(new CronValue(0, null));
            for (var x = av; x <= bv; x += step) if (!HasNum(x)) stack.Add(new CronValue(x, null));
        }
        // CronField.validate(): ranges, L forms, and duplicates (except a duplicate 0).
        var seen = new HashSet<long>();
        foreach (var v in stack)
        {
            if (v.Str is { } s)
            {
                var digits = s.Length - 1;
                if (!(s.EndsWith('L') && digits <= 2 && s[..digits].All(char.IsAsciiDigit)))
                    throw new Exception($"Validation error, got value {s} expected range {min}-{max} or chars L");
            }
            else
            {
                if (v.Num < min || v.Num > max) throw new Exception($"Validation error, got value {v.Num} expected range {min}-{max}");
                if (!seen.Add(v.Num) && v.Num != 0) throw new Exception($"Validation error, duplicate values found: {v.Num}");
            }
        }
        if (stack.Count == 0) throw new Exception("Validation error, values contains no values");
        return stack;
    }

    static readonly int[] CronDaysInMonthMax = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    /// <summary>Parse a cron expression the way cron-parser 5.5.0 does (non-strict, hashSeed set). Throws on error.</summary>
    public static CronParsed CronParseExpression(string expr)
    {
        var expression = expr.Trim();
        if (expression.Length == 0) throw new Exception("cron expression is empty");
        if (expression.Length > CronMaxExpressionLength) throw new Exception($"cron expression exceeds {CronMaxExpressionLength} characters");
        expression = expression switch
        {
            "@yearly" or "@annually" => "0 0 0 1 1 *",
            "@monthly" => "0 0 0 1 * *",
            "@weekly" => "0 0 0 * * 0",
            "@daily" => "0 0 0 * * *",
            "@hourly" => "0 0 * * * *",
            "@minutely" => "0 * * * * *",
            "@secondly" => "* * * * * *",
            "@weekdays" => "0 0 0 * * 1-5",
            "@weekends" => "0 0 0 * * 0,6",
            _ => expression,
        };
        var atoms = expression.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries).ToList();
        if (atoms.Count > 6) throw new Exception("Invalid cron expression, too many fields");
        string[] defaults = ["*", "*", "*", "*", "*", "0"];
        if (atoms.Count < 6) atoms.InsertRange(0, defaults[atoms.Count..]);
        var rand = new CronMulberry32(CronHashSeed);
        var second = CronParseField(CronUnit.Second, atoms[0], rand.Next());
        var minute = CronParseField(CronUnit.Minute, atoms[1], rand.Next());
        var hour = CronParseField(CronUnit.Hour, atoms[2], rand.Next());
        var month = CronParseField(CronUnit.Month, atoms[4], rand.Next());
        var dom = CronParseField(CronUnit.DayOfMonth, atoms[3], rand.Next());
        var rawDow = atoms[5];
        var hashParts = rawDow.Split('#');
        string dowSrc;
        var nth = 0;
        if (hashParts.Length <= 1) dowSrc = rawDow;
        else
        {
            var special = rawDow.FirstOrDefault(c => c is ',' or '-' or '/');
            if (special != default) throw new Exception($"Constraint error, invalid dayOfWeek `#` and `{special}` special characters are incompatible");
            var n = CronJsNumber(hashParts[^1]);
            if (hashParts.Length != 2 || n is not (>= 1 and <= 5)) throw new Exception("Constraint error, invalid dayOfWeek occurrence number (#)");
            dowSrc = hashParts[0];
            nth = (int)n.Value;
        }
        var dow = CronParseField(CronUnit.DayOfWeek, dowSrc, rand.Next());

        var p = new CronParsed
        {
            DomWildcard = atoms[3] is "*" or "?",
            DowWildcard = rawDow is "*" or "?",
            Nth = nth,
        };
        IEnumerable<long> Nums(List<CronValue> vs) => vs.Where(v => v.Str is null).Select(v => v.Num);
        foreach (var n in Nums(second)) p.Seconds[n] = true;
        foreach (var n in Nums(minute)) p.Minutes[n] = true;
        foreach (var n in Nums(hour)) p.Hours[n] = true;
        foreach (var n in Nums(month)) p.Months[n] = true;
        foreach (var n in Nums(dom)) p.Dom[n] = true;
        p.DomLast = atoms[3].Contains('L') || dom.Any(v => v.Str is not null);
        foreach (var n in Nums(dow)) p.Dow[n % 7] = true;
        p.DowHasLast = dowSrc.Contains('L') || dow.Any(v => v.Str is not null);
        if (p.DowHasLast)
        {
            // cron-parser reads the first character of every value as the weekday.
            foreach (var v in dow)
            {
                var first = v.Str is { } s ? s[0] : v.Num.ToString()[0];
                if (char.IsAsciiDigit(first)) p.DowLast[(first - '0') % 7] = true;
                else p.DowLastInvalid = true;
            }
        }
        // "Invalid explicit day of month definition": one month and a day it doesn't have.
        var monthNums = Nums(month).ToList();
        if (monthNums.Count == 1 && !p.DomLast)
        {
            var domNums = Nums(dom).OrderBy(x => x).ToList();
            if (domNums.Count > 0 && domNums[0] > CronDaysInMonthMax[monthNums[0] - 1])
                throw new Exception("Invalid explicit day of month definition");
        }
        return p;
    }

    static bool CronIsLeap(long y) => y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);

    static long CronDaysInMonth(long y, long m) => m switch
    {
        2 => CronIsLeap(y) ? 29 : 28,
        4 or 6 or 9 or 11 => 30,
        _ => 31,
    };

    static long CronFloorDiv(long a, long b) => a / b - ((a % b != 0) && ((a < 0) ^ (b < 0)) ? 1 : 0);
    static long CronFloorMod(long a, long b) => a - CronFloorDiv(a, b) * b;

    /// <summary>Days since 1970-01-01 (Howard Hinnant's algorithm).</summary>
    static long CronDaysFromCivil(long y, long m, long d)
    {
        y -= m <= 2 ? 1 : 0;
        var era = (y >= 0 ? y : y - 399) / 400;
        var yoe = y - era * 400;
        var doy = (153 * (m + (m > 2 ? -3 : 9)) + 2) / 5 + d - 1;
        var doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        return era * 146097 + doe - 719468;
    }

    static (long Y, long M, long D) CronCivilFromDays(long z)
    {
        z += 719468;
        var era = (z >= 0 ? z : z - 146096) / 146097;
        var doe = z - era * 146097;
        var yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        var y = yoe + era * 400;
        var doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        var mp = (5 * doy + 2) / 153;
        var d = doy - (153 * mp + 2) / 5 + 1;
        var m = mp < 10 ? mp + 3 : mp - 9;
        return (m <= 2 ? y + 1 : y, m, d);
    }

    /// <summary>0 = Sunday.</summary>
    static long CronWeekday(long days) => CronFloorMod(days + 4, 7);

    /// <summary>cron-parser's #matchDayOfMonth. Throws where it throws (a bare L in day-of-week).</summary>
    static bool CronMatchDayOfMonth(CronParsed p, long y, long m, long d)
    {
        var wd = (int)CronWeekday(CronDaysFromCivil(y, m, d));
        var last = CronDaysInMonth(y, m);
        var matchedDom = p.Dom[d] || (p.DomLast && d == last);
        var matchedDow = p.Dow[wd];
        if (!matchedDow && p.DowHasLast)
        {
            if (p.DowLastInvalid) throw new Exception("Invalid last weekday of the month expression");
            matchedDow = p.DowLast[wd] && d > last - 7;
        }
        var restrictedDom = !p.DomWildcard;
        var restrictedDow = !p.DowWildcard;
        return (restrictedDom && restrictedDow && (matchedDom || matchedDow))
            || (matchedDom && !restrictedDow)
            || (p.DomWildcard && !p.DowWildcard && matchedDow);
    }

    static long? CronNearestAfter(bool[] set, long current)
    {
        for (var v = Math.Max(current + 1, 0); v < set.Length; v++) if (set[v]) return v;
        return null;
    }

    static long CronMinOf(bool[] set) => Math.Max(0, Array.IndexOf(set, true));

    enum CronStep { Month, Day, Hour, Minute, Second }

    /// <summary>
    /// A port of cron-parser's CronDate: an instant in a zone, moved the way luxon's DateTime
    /// arithmetic moves it (so DST behaves exactly like the TS package).
    /// </summary>
    sealed class CronDate(CronZone zone, long ts)
    {
        public long Ts = ts;
        /// <summary>The UTC offset luxon carries with the value, in milliseconds.</summary>
        public long O = zone.OffsetAt(CronFloorDiv(ts, 1000)) * 1000L;
        public long? DstStart, DstEnd;

        public long OffsetMs(long t) => zone.OffsetAt(CronFloorDiv(t, 1000)) * 1000L;

        public CronDate Copy() => new(zone, Ts) { O = O, DstStart = DstStart, DstEnd = DstEnd };

        /// <summary>luxon's fixOffset: resolve a local timestamp with an offset guess.</summary>
        (long, long) FixOffset(long local, long o)
        {
            var utc = local - o;
            var o2 = OffsetMs(utc);
            if (o == o2) return (utc, o);
            utc -= o2 - o;
            var o3 = OffsetMs(utc);
            if (o2 == o3) return (utc, o2);
            return (local - Math.Min(o2, o3), Math.Max(o2, o3));
        }

        public struct Fields
        {
            public long Year, Month, Day, Hour, Minute, Second, Ms;
        }

        public Fields Local()
        {
            var l = Ts + O;
            var days = CronFloorDiv(l, 86_400_000);
            var rem = CronFloorMod(l, 86_400_000);
            var (y, m, d) = CronCivilFromDays(days);
            return new Fields { Year = y, Month = m, Day = d, Hour = rem / 3_600_000, Minute = rem / 60_000 % 60, Second = rem / 1000 % 60, Ms = rem % 1000 };
        }

        static long LocalTs(Fields c)
        {
            // Date.UTC, which normalizes overflowing fields.
            var y = c.Year + CronFloorDiv(c.Month - 1, 12);
            var m = CronFloorMod(c.Month - 1, 12) + 1;
            return (CronDaysFromCivil(y, m, 1) + c.Day - 1) * 86_400_000 + c.Hour * 3_600_000 + c.Minute * 60_000 + c.Second * 1000 + c.Ms;
        }

        public void SetLocal(Fields c) => (Ts, O) = FixOffset(LocalTs(c), O);

        /// <summary>luxon plus with calendar units.</summary>
        public void PlusCalendar(long years, long months, long days)
        {
            var c = Local();
            c.Year += years;
            c.Month += months;
            var ny = c.Year + CronFloorDiv(c.Month - 1, 12);
            var nm = CronFloorMod(c.Month - 1, 12) + 1;
            c.Day = Math.Min(c.Day, CronDaysInMonth(ny, nm)) + days;
            SetLocal(c);
        }

        /// <summary>luxon plus with time units: absolute.</summary>
        public void PlusMs(long ms)
        {
            Ts += ms;
            O = OffsetMs(Ts);
        }

        public void StartOfMonth() { var c = Local(); c.Day = 1; c.Hour = c.Minute = c.Second = c.Ms = 0; SetLocal(c); }
        public void StartOfDay() { var c = Local(); c.Hour = c.Minute = c.Second = c.Ms = 0; SetLocal(c); }
        public void StartOfHour() { var c = Local(); c.Minute = c.Second = c.Ms = 0; SetLocal(c); }
        public void StartOfMinute() { var c = Local(); c.Second = c.Ms = 0; SetLocal(c); }
        public void SetHours(long h) { var c = Local(); c.Hour = h; SetLocal(c); }
        public void SetMinutes(long m) { var c = Local(); c.Minute = m; SetLocal(c); }
        public void SetSeconds(long s) { var c = Local(); c.Second = s; SetLocal(c); }
        public void SetMilliseconds(long ms) { var c = Local(); c.Ms = ms; SetLocal(c); }

        public void Add(CronStep unit)
        {
            switch (unit)
            {
                case CronStep.Month: PlusCalendar(0, 1, 0); StartOfMonth(); break;
                case CronStep.Day: PlusCalendar(0, 0, 1); StartOfDay(); break;
                case CronStep.Hour: PlusMs(3_600_000); StartOfHour(); break;
                case CronStep.Minute: PlusMs(60_000); StartOfMinute(); break;
                default: PlusMs(1000); break;
            }
        }

        /// <summary>applyDateOperation(Add, unit, hoursLength): notes DST jumps for hour-level steps.</summary>
        public void Apply(CronStep unit, int hoursLength)
        {
            if (unit is CronStep.Month or CronStep.Day)
            {
                Add(unit);
                return;
            }
            var previous = Local().Hour;
            Add(unit);
            var l = Local();
            var diff = l.Hour - previous;
            if (diff == 2)
            {
                if (hoursLength != 24) DstStart = l.Hour;
            }
            else if (diff == 0 && l.Minute == 0 && l.Second == 0 && hoursLength != 24) DstEnd = l.Hour;
        }
    }

    const int CronLoopLimit = 10_000;

    /// <summary>A port of cron-parser's CronExpression#findSchedule (forward). Throws where it throws.</summary>
    static long CronFindSchedule(CronParsed p, CronZone zone, long currentMs)
    {
        var date = new CronDate(zone, currentMs);
        var start = date.Ts;
        var hoursLength = p.Hours.Count(b => b);
        (long, long, long)? cacheKey = null;
        var cacheValue = false;
        var stepCount = 0;
        while (++stepCount < CronLoopLimit)
        {
            var l = date.Local();
            if (!CronMatchDayOfMonth(p, l.Year, l.Month, l.Day)) { date.Apply(CronStep.Day, hoursLength); continue; }
            if (!(p.Nth == 0 || (l.Day + 6) / 7 == p.Nth)) { date.Apply(CronStep.Day, hoursLength); continue; }
            if (!p.Months[l.Month]) { date.Apply(CronStep.Month, hoursLength); continue; }

            // #matchHour
            var currentHour = l.Hour;
            if (date.DstStart == currentHour)
            {
                if (!(currentHour >= 1 && p.Hours[currentHour - 1]))
                {
                    date.Add(CronStep.Hour);
                    continue;
                }
            }
            else if (date.DstEnd == currentHour)
            {
                date.DstEnd = null;
                date.Apply(CronStep.Hour, hoursLength);
                continue;
            }
            else if (!p.Hours[currentHour])
            {
                date.DstStart = null;
                if (CronNearestAfter(p.Hours, currentHour) is not { } nextHour)
                {
                    date.Apply(CronStep.Day, hoursLength);
                    continue;
                }
                // On DST transition days, step hour by hour so the DST flags get set.
                var key = (l.Year, l.Month, l.Day);
                if (cacheKey != key)
                {
                    var a = date.Copy();
                    a.StartOfDay();
                    // luxon endOf('day'): plus one day, start of day, minus 1 ms.
                    var b = date.Copy();
                    b.PlusCalendar(0, 0, 1);
                    b.StartOfDay();
                    cacheValue = a.O != date.OffsetMs(b.Ts - 1);
                    cacheKey = key;
                }
                if (cacheValue)
                {
                    for (var i = 0; i < nextHour - currentHour; i++) date.Apply(CronStep.Hour, hoursLength);
                }
                else date.SetHours(nextHour);
                date.SetMinutes(CronMinOf(p.Minutes));
                date.SetSeconds(CronMinOf(p.Seconds));
                continue;
            }

            l = date.Local();
            if (!p.Minutes[l.Minute])
            {
                if (CronNearestAfter(p.Minutes, l.Minute) is { } m)
                {
                    date.SetMinutes(m);
                    date.SetSeconds(CronMinOf(p.Seconds));
                }
                else
                {
                    date.Apply(CronStep.Hour, hoursLength);
                    date.SetMinutes(CronMinOf(p.Minutes));
                    date.SetSeconds(CronMinOf(p.Seconds));
                }
                continue;
            }
            if (!p.Seconds[l.Second])
            {
                if (CronNearestAfter(p.Seconds, l.Second) is { } s) date.SetSeconds(s);
                else
                {
                    date.Apply(CronStep.Minute, hoursLength);
                    date.SetSeconds(CronMinOf(p.Seconds));
                }
                continue;
            }
            if (start == date.Ts)
            {
                date.Apply(CronStep.Second, hoursLength);
                continue;
            }
            break;
        }
        // At the loop limit cron-parser stops without throwing and returns wherever it got to.
        if (date.Local().Ms != 0) date.SetMilliseconds(0);
        return date.Ts;
    }

    /// <summary>
    /// The first occurrence strictly after afterMicros in timezone, at whole-second precision,
    /// exactly as the TS package computes it with cron-parser (DST quirks included). null when
    /// there is none, or the zone is invalid.
    /// </summary>
    public static long? CronNextFireAfter(CronParsed parsed, long afterMicros, string timezone)
    {
        if (CronZone.For(timezone) is not { } zone) return null;
        var afterMs = afterMicros / 1000;
        if (Math.Abs(afterMs) > CronMaxSafeDateMs) return null;
        long next;
        try { next = CronFindSchedule(parsed, zone, afterMs); }
        catch (Exception) { return null; }
        if (Math.Abs(next) > CronMaxSafeDateMs) return null;
        return next * 1000;
    }

    // -----------------------------------------------------------------------
    // Time zones
    // -----------------------------------------------------------------------

    /// <summary>Whether tz names a zone (case-insensitive IANA name, UTC, or a ±HH:MM offset).</summary>
    public static bool CronIsValidTimezone(string tz) =>
        tz.Length > 0 && tz.Length <= CronMaxTimezoneLength && tz == tz.Trim() && CronZone.For(tz) is not null;

    /// <summary>The UTC offset in seconds at unixMicros in tz, or null for an unknown zone.</summary>
    public static int? CronUtcOffsetSeconds(string tz, long unixMicros) =>
        CronZone.For(tz)?.OffsetAt(CronFloorDiv(unixMicros, 1_000_000));

    sealed class CronPosixRule
    {
        public int Std;
        public bool HasDst;
        public int Dst;
        public (char Kind, long A, long B, long C) Start, End;
        public int StartTime, EndTime;

        public int OffsetAt(long t)
        {
            if (!HasDst) return Std;
            var y = CronCivilFromDays(CronFloorDiv(t, 86400)).Y;
            var events = new List<(long When, int Off)>(6);
            for (var yy = y - 1; yy <= y + 1; yy++)
            {
                events.Add((RuleDay(Start, yy) * 86400 + StartTime - Std, Dst));
                events.Add((RuleDay(End, yy) * 86400 + EndTime - Dst, Std));
            }
            events.Sort();
            var cur = events[0].Off == Dst ? Std : Dst;
            foreach (var (when, off) in events) if (when <= t) cur = off;
            return cur;
        }

        static long RuleDay((char Kind, long A, long B, long C) d, long y)
        {
            switch (d.Kind)
            {
                case 'J':
                    return CronDaysFromCivil(y, 1, 1) + d.A - 1 + (CronIsLeap(y) && d.A > 59 ? 1 : 0);
                case 'N':
                    return CronDaysFromCivil(y, 1, 1) + d.A;
                default:
                    var (m, w, wd) = (d.A, d.B, d.C);
                    var first = CronDaysFromCivil(y, m, 1);
                    var day = first + CronFloorMod(wd - CronWeekday(first), 7) + (w - 1) * 7;
                    while (day >= first + CronDaysInMonth(y, m)) day -= 7;
                    return day;
            }
        }

        public static CronPosixRule? Parse(string s)
        {
            var i = 0;
            bool Name()
            {
                if (i < s.Length && s[i] == '<')
                {
                    var j = s.IndexOf('>', i);
                    if (j < 0) return false;
                    i = j + 1;
                    return true;
                }
                var st = i;
                while (i < s.Length && char.IsAsciiLetter(s[i])) i++;
                return i - st >= 3;
            }
            int? Time()
            {
                var sign = 1;
                if (i < s.Length && (s[i] == '+' || s[i] == '-')) { sign = s[i] == '-' ? -1 : 1; i++; }
                var parts = new int[3];
                for (var k = 0; k < 3; k++)
                {
                    if (k > 0)
                    {
                        if (i >= s.Length || s[i] != ':') break;
                        i++;
                    }
                    var st = i;
                    while (i < s.Length && char.IsAsciiDigit(s[i])) i++;
                    if (i == st) return null;
                    parts[k] = int.Parse(s[st..i]);
                }
                return sign * (parts[0] * 3600 + parts[1] * 60 + parts[2]);
            }
            long Num()
            {
                var st = i;
                while (i < s.Length && char.IsAsciiDigit(s[i])) i++;
                return long.Parse(s[st..i]);
            }
            ((char, long, long, long), int)? Date()
            {
                (char, long, long, long) d;
                if (i < s.Length && s[i] == 'M')
                {
                    i++;
                    var m = Num(); i++;
                    var w = Num(); i++;
                    var wd = Num();
                    d = ('M', m, w, wd);
                }
                else if (i < s.Length && s[i] == 'J') { i++; d = ('J', Num(), 0, 0); }
                else d = ('N', Num(), 0, 0);
                var t = 7200;
                if (i < s.Length && s[i] == '/')
                {
                    i++;
                    if (Time() is not { } tt) return null;
                    t = tt;
                }
                return (d, t);
            }
            if (!Name() || Time() is not { } std) return null;
            var r = new CronPosixRule { Std = -std };
            if (i == s.Length) return r;
            if (!Name()) return null;
            r.HasDst = true;
            r.Dst = r.Std + 3600;
            if (i < s.Length && s[i] != ',')
            {
                if (Time() is not { } dst) return null;
                r.Dst = -dst;
            }
            if (i < s.Length && s[i] == ',')
            {
                i++;
                if (Date() is not { } sd) return null;
                if (i >= s.Length || s[i] != ',') return null;
                i++;
                if (Date() is not { } ed) return null;
                (r.Start, r.StartTime, r.End, r.EndTime) = (sd.Item1, sd.Item2, ed.Item1, ed.Item2);
            }
            else (r.Start, r.StartTime, r.End, r.EndTime) = (('M', 3, 2, 0), 7200, ('M', 11, 1, 0), 7200);
            return r;
        }
    }

    sealed class CronZone
    {
        int fixedOffset;
        CronPosixRule? rule;
        (long At, int Offset)[]? transitions;
        int initial;

        public int OffsetAt(long t)
        {
            if (rule is null) return fixedOffset;
            if (transitions is { Length: > 0 } tr && t < tr[^1].At)
            {
                var lo = 0;
                var hi = tr.Length;
                while (lo < hi)
                {
                    var mid = (lo + hi) / 2;
                    if (tr[mid].At <= t) lo = mid + 1;
                    else hi = mid;
                }
                return lo == 0 ? initial : tr[lo - 1].Offset;
            }
            return rule.OffsetAt(t);
        }

        public static CronZone? For(string tz)
        {
            if (tz.Length == 0 || tz.Length > CronMaxTimezoneLength || tz != tz.Trim()) return null;
            if (ParseOffsetZone(tz) is { } off) return new CronZone { fixedOffset = off };
            var lower = tz.ToLowerInvariant();
            var idx = Array.BinarySearch(TzNames, lower, StringComparer.Ordinal);
            if (idx < 0) return null;
            if (CronPosixRule.Parse(TzRules[TzNameRule[idx]]) is not { } r) return null;
            var zone = new CronZone();
            foreach (var (name, init, trans) in TzExplicit)
            {
                if (name == lower)
                {
                    zone.rule = r;
                    zone.transitions = trans;
                    zone.initial = init;
                    return zone;
                }
            }
            if (r.HasDst) zone.rule = r;
            else zone.fixedOffset = r.Std;
            return zone;
        }

        /// <summary>+HH:MM, -HH:MM, +HHMM or +HH (like ECMAScript offset time zones).</summary>
        static int? ParseOffsetZone(string tz)
        {
            if (tz.Length == 0 || (tz[0] != '+' && tz[0] != '-')) return null;
            var sign = tz[0] == '-' ? -1 : 1;
            var shapeOk = tz.Length switch
            {
                3 => true,
                5 => !tz.Contains(':'),
                6 => tz[3] == ':',
                _ => false,
            };
            var digits = tz[1..].Replace(":", "");
            if (!shapeOk || !digits.All(char.IsAsciiDigit)) return null;
            var h = int.Parse(digits[..2]);
            var m = digits.Length == 4 ? int.Parse(digits[2..]) : 0;
            if (h > 23 || m > 59) return null;
            return sign * (h * 3600 + m * 60);
        }
    }

    // BEGIN GENERATED TZ (tools/gen-tz.py, tzdata 2026c)
    /// <summary>IANA tzdata version the zone table was generated from.</summary>
    public const string TzdataVersion = "2026c";
    static readonly string[] TzRules =
    {
        "<+00>0", "<+00>0<+02>-2,M3.5.0/1,M10.5.0/3", "<+01>-1", "<+02>-2", "<+0330>-3:30", "<+03>-3",
        "<+0430>-4:30", "<+04>-4", "<+0530>-5:30", "<+0545>-5:45", "<+05>-5", "<+0630>-6:30", "<+06>-6",
        "<+07>-7", "<+0845>-8:45", "<+08>-8", "<+09>-9", "<+1030>-10:30<+11>-11,M10.1.0,M4.1.0", "<+10>-10",
        "<+11>-11", "<+11>-11<+12>,M10.1.0,M4.1.0/3", "<+1245>-12:45<+1345>,M9.5.0/2:45,M4.1.0/3:45",
        "<+12>-12", "<+13>-13", "<+14>-14", "<-01>1", "<-01>1<+00>,M3.5.0/0,M10.5.0/1", "<-02>2",
        "<-02>2<-01>,M3.5.0/-1,M10.5.0/0", "<-03>3", "<-03>3<-02>,M3.2.0,M11.1.0", "<-04>4",
        "<-04>4<-03>,M9.1.6/24,M4.1.6/24", "<-05>5", "<-06>6", "<-06>6<-05>,M9.1.6/22,M4.1.6/22", "<-07>7",
        "<-08>8", "<-0930>9:30", "<-09>9", "<-10>10", "<-11>11", "<-12>12", "ACST-9:30",
        "ACST-9:30ACDT,M10.1.0,M4.1.0/3", "AEST-10", "AEST-10AEDT,M10.1.0,M4.1.0/3",
        "AKST9AKDT,M3.2.0,M11.1.0", "AST4", "AST4ADT,M3.2.0,M11.1.0", "AWST-8", "CAT-2", "CET-1",
        "CET-1CEST,M3.5.0,M10.5.0/3", "CST-8", "CST5CDT,M3.2.0/0,M11.1.0/1", "CST6",
        "CST6CDT,M3.2.0,M11.1.0", "ChST-10", "EAT-3", "EET-2", "EET-2EEST,M3.4.4/50,M10.4.4/50",
        "EET-2EEST,M3.5.0/0,M10.5.0/0", "EET-2EEST,M3.5.0/3,M10.5.0/4", "EET-2EEST,M4.5.5/0,M10.5.4/24",
        "EST5", "EST5EDT,M3.2.0,M11.1.0", "GMT0", "GMT0BST,M3.5.0/1,M10.5.0", "GMT0IST,M3.5.0/1,M10.5.0",
        "HKT-8", "HST10", "HST10HDT,M3.2.0,M11.1.0", "IST-2IDT,M3.4.4/26,M10.5.0", "IST-5:30", "JST-9",
        "KST-9", "MET-1MEST,M3.5.0,M10.5.0/3", "MSK-3", "MST7", "MST7MDT,M3.2.0,M11.1.0",
        "NST3:30NDT,M3.2.0,M11.1.0", "NZST-12NZDT,M9.5.0,M4.1.0/3", "PKT-5", "PST-8",
        "PST8PDT,M3.2.0,M11.1.0", "SAST-2", "SST11", "UTC0", "WAT-1", "WET0WEST,M3.5.0/1,M10.5.0", "WIB-7",
        "WIT-9", "WITA-8",
    };
    /// <summary>Lowercase zone names (sorted ordinally).</summary>
    static readonly string[] TzNames =
    {
        "africa/abidjan", "africa/accra", "africa/addis_ababa", "africa/algiers", "africa/asmara",
        "africa/asmera", "africa/bamako", "africa/bangui", "africa/banjul", "africa/bissau",
        "africa/blantyre", "africa/brazzaville", "africa/bujumbura", "africa/cairo", "africa/casablanca",
        "africa/ceuta", "africa/conakry", "africa/dakar", "africa/dar_es_salaam", "africa/djibouti",
        "africa/douala", "africa/el_aaiun", "africa/freetown", "africa/gaborone", "africa/harare",
        "africa/johannesburg", "africa/juba", "africa/kampala", "africa/khartoum", "africa/kigali",
        "africa/kinshasa", "africa/lagos", "africa/libreville", "africa/lome", "africa/luanda",
        "africa/lubumbashi", "africa/lusaka", "africa/malabo", "africa/maputo", "africa/maseru",
        "africa/mbabane", "africa/mogadishu", "africa/monrovia", "africa/nairobi", "africa/ndjamena",
        "africa/niamey", "africa/nouakchott", "africa/ouagadougou", "africa/porto-novo", "africa/sao_tome",
        "africa/timbuktu", "africa/tripoli", "africa/tunis", "africa/windhoek", "america/adak",
        "america/anchorage", "america/anguilla", "america/antigua", "america/araguaina",
        "america/argentina/buenos_aires", "america/argentina/catamarca", "america/argentina/comodrivadavia",
        "america/argentina/cordoba", "america/argentina/jujuy", "america/argentina/la_rioja",
        "america/argentina/mendoza", "america/argentina/rio_gallegos", "america/argentina/salta",
        "america/argentina/san_juan", "america/argentina/san_luis", "america/argentina/tucuman",
        "america/argentina/ushuaia", "america/aruba", "america/asuncion", "america/atikokan", "america/atka",
        "america/bahia", "america/bahia_banderas", "america/barbados", "america/belem", "america/belize",
        "america/blanc-sablon", "america/boa_vista", "america/bogota", "america/boise",
        "america/buenos_aires", "america/cambridge_bay", "america/campo_grande", "america/cancun",
        "america/caracas", "america/catamarca", "america/cayenne", "america/cayman", "america/chicago",
        "america/chihuahua", "america/ciudad_juarez", "america/coral_harbour", "america/cordoba",
        "america/costa_rica", "america/coyhaique", "america/creston", "america/cuiaba", "america/curacao",
        "america/danmarkshavn", "america/dawson", "america/dawson_creek", "america/denver",
        "america/detroit", "america/dominica", "america/edmonton", "america/eirunepe", "america/el_salvador",
        "america/ensenada", "america/fort_nelson", "america/fort_wayne", "america/fortaleza",
        "america/glace_bay", "america/godthab", "america/goose_bay", "america/grand_turk", "america/grenada",
        "america/guadeloupe", "america/guatemala", "america/guayaquil", "america/guyana", "america/halifax",
        "america/havana", "america/hermosillo", "america/indiana/indianapolis", "america/indiana/knox",
        "america/indiana/marengo", "america/indiana/petersburg", "america/indiana/tell_city",
        "america/indiana/vevay", "america/indiana/vincennes", "america/indiana/winamac",
        "america/indianapolis", "america/inuvik", "america/iqaluit", "america/jamaica", "america/jujuy",
        "america/juneau", "america/kentucky/louisville", "america/kentucky/monticello", "america/knox_in",
        "america/kralendijk", "america/la_paz", "america/lima", "america/los_angeles", "america/louisville",
        "america/lower_princes", "america/maceio", "america/managua", "america/manaus", "america/marigot",
        "america/martinique", "america/matamoros", "america/mazatlan", "america/mendoza",
        "america/menominee", "america/merida", "america/metlakatla", "america/mexico_city",
        "america/miquelon", "america/moncton", "america/monterrey", "america/montevideo", "america/montreal",
        "america/montserrat", "america/nassau", "america/new_york", "america/nipigon", "america/nome",
        "america/noronha", "america/north_dakota/beulah", "america/north_dakota/center",
        "america/north_dakota/new_salem", "america/nuuk", "america/ojinaga", "america/panama",
        "america/pangnirtung", "america/paramaribo", "america/phoenix", "america/port-au-prince",
        "america/port_of_spain", "america/porto_acre", "america/porto_velho", "america/puerto_rico",
        "america/punta_arenas", "america/rainy_river", "america/rankin_inlet", "america/recife",
        "america/regina", "america/resolute", "america/rio_branco", "america/rosario",
        "america/santa_isabel", "america/santarem", "america/santiago", "america/santo_domingo",
        "america/sao_paulo", "america/scoresbysund", "america/shiprock", "america/sitka",
        "america/st_barthelemy", "america/st_johns", "america/st_kitts", "america/st_lucia",
        "america/st_thomas", "america/st_vincent", "america/swift_current", "america/tegucigalpa",
        "america/thule", "america/thunder_bay", "america/tijuana", "america/toronto", "america/tortola",
        "america/vancouver", "america/virgin", "america/whitehorse", "america/winnipeg", "america/yakutat",
        "america/yellowknife", "antarctica/casey", "antarctica/davis", "antarctica/dumontdurville",
        "antarctica/macquarie", "antarctica/mawson", "antarctica/mcmurdo", "antarctica/palmer",
        "antarctica/rothera", "antarctica/south_pole", "antarctica/syowa", "antarctica/troll",
        "antarctica/vostok", "arctic/longyearbyen", "asia/aden", "asia/almaty", "asia/amman", "asia/anadyr",
        "asia/aqtau", "asia/aqtobe", "asia/ashgabat", "asia/ashkhabad", "asia/atyrau", "asia/baghdad",
        "asia/bahrain", "asia/baku", "asia/bangkok", "asia/barnaul", "asia/beirut", "asia/bishkek",
        "asia/brunei", "asia/calcutta", "asia/chita", "asia/choibalsan", "asia/chongqing", "asia/chungking",
        "asia/colombo", "asia/dacca", "asia/damascus", "asia/dhaka", "asia/dili", "asia/dubai",
        "asia/dushanbe", "asia/famagusta", "asia/gaza", "asia/harbin", "asia/hebron", "asia/ho_chi_minh",
        "asia/hong_kong", "asia/hovd", "asia/irkutsk", "asia/istanbul", "asia/jakarta", "asia/jayapura",
        "asia/jerusalem", "asia/kabul", "asia/kamchatka", "asia/karachi", "asia/kashgar", "asia/kathmandu",
        "asia/katmandu", "asia/khandyga", "asia/kolkata", "asia/krasnoyarsk", "asia/kuala_lumpur",
        "asia/kuching", "asia/kuwait", "asia/macao", "asia/macau", "asia/magadan", "asia/makassar",
        "asia/manila", "asia/muscat", "asia/nicosia", "asia/novokuznetsk", "asia/novosibirsk", "asia/omsk",
        "asia/oral", "asia/phnom_penh", "asia/pontianak", "asia/pyongyang", "asia/qatar", "asia/qostanay",
        "asia/qyzylorda", "asia/rangoon", "asia/riyadh", "asia/saigon", "asia/sakhalin", "asia/samarkand",
        "asia/seoul", "asia/shanghai", "asia/singapore", "asia/srednekolymsk", "asia/taipei",
        "asia/tashkent", "asia/tbilisi", "asia/tehran", "asia/tel_aviv", "asia/thimbu", "asia/thimphu",
        "asia/tokyo", "asia/tomsk", "asia/ujung_pandang", "asia/ulaanbaatar", "asia/ulan_bator",
        "asia/urumqi", "asia/ust-nera", "asia/vientiane", "asia/vladivostok", "asia/yakutsk", "asia/yangon",
        "asia/yekaterinburg", "asia/yerevan", "atlantic/azores", "atlantic/bermuda", "atlantic/canary",
        "atlantic/cape_verde", "atlantic/faeroe", "atlantic/faroe", "atlantic/jan_mayen", "atlantic/madeira",
        "atlantic/reykjavik", "atlantic/south_georgia", "atlantic/st_helena", "atlantic/stanley",
        "australia/act", "australia/adelaide", "australia/brisbane", "australia/broken_hill",
        "australia/canberra", "australia/currie", "australia/darwin", "australia/eucla", "australia/hobart",
        "australia/lhi", "australia/lindeman", "australia/lord_howe", "australia/melbourne",
        "australia/north", "australia/nsw", "australia/perth", "australia/queensland", "australia/south",
        "australia/sydney", "australia/tasmania", "australia/victoria", "australia/west",
        "australia/yancowinna", "brazil/acre", "brazil/denoronha", "brazil/east", "brazil/west",
        "canada/atlantic", "canada/central", "canada/eastern", "canada/mountain", "canada/newfoundland",
        "canada/pacific", "canada/saskatchewan", "canada/yukon", "cet", "chile/continental",
        "chile/easterisland", "cst6cdt", "cuba", "eet", "egypt", "eire", "est", "est5edt", "etc/gmt",
        "etc/gmt+0", "etc/gmt+1", "etc/gmt+10", "etc/gmt+11", "etc/gmt+12", "etc/gmt+2", "etc/gmt+3",
        "etc/gmt+4", "etc/gmt+5", "etc/gmt+6", "etc/gmt+7", "etc/gmt+8", "etc/gmt+9", "etc/gmt-0",
        "etc/gmt-1", "etc/gmt-10", "etc/gmt-11", "etc/gmt-12", "etc/gmt-13", "etc/gmt-14", "etc/gmt-2",
        "etc/gmt-3", "etc/gmt-4", "etc/gmt-5", "etc/gmt-6", "etc/gmt-7", "etc/gmt-8", "etc/gmt-9",
        "etc/gmt0", "etc/greenwich", "etc/uct", "etc/universal", "etc/utc", "etc/zulu", "europe/amsterdam",
        "europe/andorra", "europe/astrakhan", "europe/athens", "europe/belfast", "europe/belgrade",
        "europe/berlin", "europe/bratislava", "europe/brussels", "europe/bucharest", "europe/budapest",
        "europe/busingen", "europe/chisinau", "europe/copenhagen", "europe/dublin", "europe/gibraltar",
        "europe/guernsey", "europe/helsinki", "europe/isle_of_man", "europe/istanbul", "europe/jersey",
        "europe/kaliningrad", "europe/kiev", "europe/kirov", "europe/kyiv", "europe/lisbon",
        "europe/ljubljana", "europe/london", "europe/luxembourg", "europe/madrid", "europe/malta",
        "europe/mariehamn", "europe/minsk", "europe/monaco", "europe/moscow", "europe/nicosia",
        "europe/oslo", "europe/paris", "europe/podgorica", "europe/prague", "europe/riga", "europe/rome",
        "europe/samara", "europe/san_marino", "europe/sarajevo", "europe/saratov", "europe/simferopol",
        "europe/skopje", "europe/sofia", "europe/stockholm", "europe/tallinn", "europe/tirane",
        "europe/tiraspol", "europe/ulyanovsk", "europe/uzhgorod", "europe/vaduz", "europe/vatican",
        "europe/vienna", "europe/vilnius", "europe/volgograd", "europe/warsaw", "europe/zagreb",
        "europe/zaporozhye", "europe/zurich", "gb", "gb-eire", "gmt", "gmt+0", "gmt-0", "gmt0", "greenwich",
        "hongkong", "hst", "iceland", "indian/antananarivo", "indian/chagos", "indian/christmas",
        "indian/cocos", "indian/comoro", "indian/kerguelen", "indian/mahe", "indian/maldives",
        "indian/mauritius", "indian/mayotte", "indian/reunion", "iran", "israel", "jamaica", "japan",
        "kwajalein", "libya", "met", "mexico/bajanorte", "mexico/bajasur", "mexico/general", "mst",
        "mst7mdt", "navajo", "nz", "nz-chat", "pacific/apia", "pacific/auckland", "pacific/bougainville",
        "pacific/chatham", "pacific/chuuk", "pacific/easter", "pacific/efate", "pacific/enderbury",
        "pacific/fakaofo", "pacific/fiji", "pacific/funafuti", "pacific/galapagos", "pacific/gambier",
        "pacific/guadalcanal", "pacific/guam", "pacific/honolulu", "pacific/johnston", "pacific/kanton",
        "pacific/kiritimati", "pacific/kosrae", "pacific/kwajalein", "pacific/majuro", "pacific/marquesas",
        "pacific/midway", "pacific/nauru", "pacific/niue", "pacific/norfolk", "pacific/noumea",
        "pacific/pago_pago", "pacific/palau", "pacific/pitcairn", "pacific/pohnpei", "pacific/ponape",
        "pacific/port_moresby", "pacific/rarotonga", "pacific/saipan", "pacific/samoa", "pacific/tahiti",
        "pacific/tarawa", "pacific/tongatapu", "pacific/truk", "pacific/wake", "pacific/wallis",
        "pacific/yap", "poland", "portugal", "prc", "pst8pdt", "roc", "rok", "singapore", "turkey", "uct",
        "universal", "us/alaska", "us/aleutian", "us/arizona", "us/central", "us/east-indiana", "us/eastern",
        "us/hawaii", "us/indiana-starke", "us/michigan", "us/mountain", "us/pacific", "us/samoa", "utc",
        "w-su", "wet", "zulu",
    };
    /// <summary>Index into TzRules for each entry of TzNames.</summary>
    static readonly ushort[] TzNameRule =
    {
        67, 67, 59, 52, 59, 59, 67, 89, 67, 67, 51, 89, 51, 64, 0, 53, 67, 67, 59, 59, 89, 0, 67, 51, 51, 86,
        51, 59, 51, 51, 89, 89, 89, 67, 89, 51, 51, 89, 51, 86, 86, 59, 67, 59, 89, 89, 67, 67, 89, 67, 67,
        60, 52, 51, 72, 47, 48, 48, 29, 29, 29, 29, 29, 29, 29, 29, 29, 29, 29, 29, 29, 29, 48, 29, 65, 72,
        29, 56, 48, 29, 56, 48, 31, 33, 80, 29, 80, 31, 65, 31, 29, 29, 65, 57, 56, 80, 65, 29, 56, 29, 79,
        31, 48, 67, 79, 79, 80, 66, 48, 56, 33, 56, 85, 79, 66, 29, 49, 28, 49, 66, 48, 48, 56, 33, 31, 49,
        55, 79, 66, 57, 66, 66, 57, 66, 66, 66, 66, 80, 66, 65, 29, 47, 66, 66, 57, 48, 31, 33, 85, 66, 48,
        29, 56, 31, 48, 48, 57, 79, 29, 57, 56, 47, 56, 30, 49, 56, 29, 66, 48, 66, 66, 66, 47, 27, 57, 57,
        57, 28, 57, 65, 66, 29, 79, 66, 48, 33, 31, 48, 29, 57, 57, 29, 56, 57, 33, 29, 85, 29, 32, 48, 29,
        28, 80, 47, 48, 81, 48, 48, 48, 48, 56, 56, 49, 66, 85, 66, 48, 79, 48, 79, 57, 47, 56, 15, 13, 18,
        46, 10, 82, 29, 29, 82, 5, 1, 10, 53, 5, 10, 5, 22, 10, 10, 10, 10, 10, 5, 5, 7, 13, 13, 62, 12, 15,
        74, 16, 15, 54, 54, 8, 12, 5, 12, 16, 7, 10, 63, 61, 54, 61, 13, 70, 13, 15, 5, 91, 92, 73, 6, 22,
        83, 12, 9, 9, 16, 74, 13, 15, 15, 5, 54, 54, 19, 93, 84, 7, 63, 13, 13, 12, 10, 13, 91, 76, 5, 10,
        10, 11, 5, 13, 19, 10, 76, 54, 15, 19, 54, 10, 7, 4, 73, 12, 12, 75, 13, 93, 15, 15, 12, 18, 13, 18,
        16, 11, 10, 7, 26, 49, 90, 25, 90, 90, 53, 90, 67, 27, 67, 29, 46, 44, 45, 44, 46, 46, 43, 14, 46,
        17, 45, 17, 46, 43, 46, 50, 45, 44, 46, 46, 46, 50, 44, 33, 27, 29, 31, 49, 57, 66, 56, 81, 79, 56,
        79, 53, 32, 35, 57, 55, 63, 64, 69, 65, 66, 67, 67, 25, 40, 41, 42, 27, 29, 31, 33, 34, 36, 37, 39,
        67, 2, 18, 19, 22, 23, 24, 3, 5, 7, 10, 12, 13, 15, 16, 67, 67, 88, 88, 88, 88, 53, 53, 7, 63, 68,
        53, 53, 53, 53, 63, 53, 53, 63, 53, 69, 53, 68, 63, 68, 5, 68, 60, 63, 78, 63, 90, 53, 68, 53, 53,
        53, 63, 5, 53, 78, 63, 53, 53, 53, 53, 63, 53, 7, 53, 53, 7, 78, 53, 63, 53, 63, 53, 63, 7, 63, 53,
        53, 53, 63, 78, 53, 53, 63, 53, 68, 68, 67, 67, 67, 67, 67, 70, 71, 67, 59, 12, 13, 11, 59, 10, 7,
        10, 7, 59, 7, 4, 73, 65, 75, 22, 60, 77, 85, 79, 56, 79, 80, 80, 82, 21, 23, 82, 19, 21, 18, 35, 19,
        23, 23, 22, 22, 34, 39, 19, 58, 71, 71, 23, 24, 19, 22, 22, 38, 87, 22, 41, 20, 19, 87, 16, 37, 19,
        19, 18, 40, 58, 87, 40, 22, 23, 18, 22, 22, 18, 53, 90, 54, 85, 54, 76, 15, 5, 88, 88, 47, 72, 79,
        57, 66, 66, 71, 57, 66, 80, 85, 87, 88, 78, 90, 88,
    };
    /// <summary>Zones whose tzdata transitions from 2025 on aren't expressible by their rule.</summary>
    static readonly (string Name, int Initial, (long At, int Offset)[] Transitions)[] TzExplicit =
    {
        ("africa/casablanca", 3600, new (long, int)[]
        {
            (1740276000, 0), (1743904800, 3600), (1771120800, 0), (1774144800, 3600), (1789866000, 0),
            (2147483647, 0),
        }),
        ("africa/el_aaiun", 3600, new (long, int)[]
        {
            (1740276000, 0), (1743904800, 3600), (1771120800, 0), (1774144800, 3600), (1789866000, 0),
            (2147483647, 0),
        }),
        ("america/edmonton", -25200, new (long, int)[]
        {
            (1741510800, -21600), (1762070400, -25200), (1772960400, -21600), (1793520000, -21600),
        }),
        ("america/vancouver", -28800, new (long, int)[]
        {
            (1741514400, -25200), (1762074000, -28800), (1772964000, -25200), (1793523600, -25200),
        }),
        ("america/yellowknife", -25200, new (long, int)[]
        {
            (1741510800, -21600), (1762070400, -25200), (1772960400, -21600), (1793520000, -21600),
        }),
        ("asia/gaza", 7200, new (long, int)[]
        {
            (1744416000, 10800), (1761346800, 7200), (1774656000, 10800), (1792796400, 7200),
            (1806105600, 10800), (1824850800, 7200), (1837555200, 10800), (1856300400, 7200),
            (1869004800, 10800), (1887750000, 7200), (1901059200, 10800), (1919199600, 7200),
            (1932508800, 10800), (1950649200, 7200), (1963958400, 10800), (1982703600, 7200),
            (1995408000, 10800), (2014153200, 7200), (2026857600, 10800), (2045602800, 7200),
            (2058307200, 10800), (2077052400, 7200), (2090361600, 10800), (2107897200, 7200),
            (2121811200, 10800), (2138742000, 7200), (2153260800, 10800), (2168982000, 7200),
            (2184710400, 10800), (2199826800, 7200), (2216160000, 10800), (2230066800, 7200),
            (2234304000, 10800), (2234905200, 7200), (2248214400, 10800), (2260911600, 7200),
            (2264544000, 10800), (2266354800, 7200), (2279664000, 10800), (2291756400, 7200),
            (2295388800, 10800), (2297804400, 7200), (2311113600, 10800), (2321996400, 7200),
            (2326233600, 10800), (2329254000, 7200), (2342563200, 10800), (2352841200, 7200),
            (2356473600, 10800), (2361308400, 7200), (2374012800, 10800), (2383686000, 7200),
            (2387318400, 10800), (2392758000, 7200), (2405462400, 10800), (2413926000, 7200),
            (2418163200, 10800), (2424207600, 7200), (2437516800, 10800), (2444770800, 7200),
            (2448403200, 10800), (2455657200, 7200), (2468966400, 10800), (2475010800, 7200),
            (2479248000, 10800), (2487106800, 7200), (2500416000, 10800), (2505855600, 7200),
            (2509488000, 10800), (2519161200, 7200), (2531865600, 10800), (2536700400, 7200),
            (2540332800, 10800), (2550610800, 7200), (2563315200, 10800), (2566940400, 7200),
            (2571177600, 10800), (2582060400, 7200), (2595369600, 10800), (2597785200, 7200),
            (2601417600, 10800), (2613510000, 7200), (2626819200, 10800), (2628025200, 7200),
            (2632262400, 10800), (2644959600, 7200), (2658268800, 10800), (2658870000, 7200),
            (2663107200, 10800), (2676409200, 7200), (2693347200, 10800), (2708463600, 7200),
            (2724192000, 10800), (2739913200, 7200), (2754432000, 10800), (2771362800, 7200),
            (2785276800, 10800), (2802812400, 7200), (2816121600, 10800), (2834262000, 7200),
            (2847571200, 10800), (2866316400, 7200), (2879020800, 10800), (2897766000, 7200),
            (2910470400, 10800), (2929215600, 7200), (2941920000, 10800), (2960665200, 7200),
            (2973974400, 10800), (2992114800, 7200), (3005424000, 10800), (3023564400, 7200),
            (3036873600, 10800), (3055618800, 7200), (3068323200, 10800), (3087068400, 7200),
            (3099772800, 10800), (3117913200, 7200), (3131827200, 10800), (3148758000, 7200),
            (3163276800, 10800), (3179602800, 7200), (3194726400, 10800), (3209842800, 7200),
            (3226176000, 10800), (3240687600, 7200), (3244320000, 10800), (3244921200, 7200),
            (3257625600, 10800), (3271532400, 7200), (3275164800, 10800), (3276370800, 7200),
            (3289075200, 10800), (3301772400, 7200), (3306009600, 10800), (3307820400, 7200),
            (3321129600, 10800), (3332617200, 7200), (3336249600, 10800), (3339270000, 7200),
            (3352579200, 10800), (3362857200, 7200), (3367094400, 10800), (3370719600, 7200),
            (3384028800, 10800), (3393702000, 7200), (3397939200, 10800), (3402774000, 7200),
            (3415478400, 10800), (3424546800, 7200), (3428179200, 10800), (3434223600, 7200),
            (3446928000, 10800), (3454786800, 7200), (3459024000, 10800), (3465673200, 7200),
            (3478982400, 10800), (3485631600, 7200), (3489264000, 10800), (3497122800, 7200),
            (3510432000, 10800), (3516476400, 7200), (3520108800, 10800), (3528572400, 7200),
            (3541881600, 10800), (3546716400, 7200), (3550953600, 10800), (3560022000, 7200),
            (3573331200, 10800), (3577561200, 7200), (3581193600, 10800), (3592076400, 7200),
            (3604780800, 10800), (3607801200, 7200), (3612038400, 10800), (3623526000, 7200),
            (3636230400, 10800), (3638646000, 7200), (3642883200, 10800), (3654975600, 7200),
            (3668284800, 10800), (3669490800, 7200), (3673123200, 10800), (3686425200, 7200),
        }),
        ("asia/hebron", 7200, new (long, int)[]
        {
            (1744416000, 10800), (1761346800, 7200), (1774656000, 10800), (1792796400, 7200),
            (1806105600, 10800), (1824850800, 7200), (1837555200, 10800), (1856300400, 7200),
            (1869004800, 10800), (1887750000, 7200), (1901059200, 10800), (1919199600, 7200),
            (1932508800, 10800), (1950649200, 7200), (1963958400, 10800), (1982703600, 7200),
            (1995408000, 10800), (2014153200, 7200), (2026857600, 10800), (2045602800, 7200),
            (2058307200, 10800), (2077052400, 7200), (2090361600, 10800), (2107897200, 7200),
            (2121811200, 10800), (2138742000, 7200), (2153260800, 10800), (2168982000, 7200),
            (2184710400, 10800), (2199826800, 7200), (2216160000, 10800), (2230066800, 7200),
            (2234304000, 10800), (2234905200, 7200), (2248214400, 10800), (2260911600, 7200),
            (2264544000, 10800), (2266354800, 7200), (2279664000, 10800), (2291756400, 7200),
            (2295388800, 10800), (2297804400, 7200), (2311113600, 10800), (2321996400, 7200),
            (2326233600, 10800), (2329254000, 7200), (2342563200, 10800), (2352841200, 7200),
            (2356473600, 10800), (2361308400, 7200), (2374012800, 10800), (2383686000, 7200),
            (2387318400, 10800), (2392758000, 7200), (2405462400, 10800), (2413926000, 7200),
            (2418163200, 10800), (2424207600, 7200), (2437516800, 10800), (2444770800, 7200),
            (2448403200, 10800), (2455657200, 7200), (2468966400, 10800), (2475010800, 7200),
            (2479248000, 10800), (2487106800, 7200), (2500416000, 10800), (2505855600, 7200),
            (2509488000, 10800), (2519161200, 7200), (2531865600, 10800), (2536700400, 7200),
            (2540332800, 10800), (2550610800, 7200), (2563315200, 10800), (2566940400, 7200),
            (2571177600, 10800), (2582060400, 7200), (2595369600, 10800), (2597785200, 7200),
            (2601417600, 10800), (2613510000, 7200), (2626819200, 10800), (2628025200, 7200),
            (2632262400, 10800), (2644959600, 7200), (2658268800, 10800), (2658870000, 7200),
            (2663107200, 10800), (2676409200, 7200), (2693347200, 10800), (2708463600, 7200),
            (2724192000, 10800), (2739913200, 7200), (2754432000, 10800), (2771362800, 7200),
            (2785276800, 10800), (2802812400, 7200), (2816121600, 10800), (2834262000, 7200),
            (2847571200, 10800), (2866316400, 7200), (2879020800, 10800), (2897766000, 7200),
            (2910470400, 10800), (2929215600, 7200), (2941920000, 10800), (2960665200, 7200),
            (2973974400, 10800), (2992114800, 7200), (3005424000, 10800), (3023564400, 7200),
            (3036873600, 10800), (3055618800, 7200), (3068323200, 10800), (3087068400, 7200),
            (3099772800, 10800), (3117913200, 7200), (3131827200, 10800), (3148758000, 7200),
            (3163276800, 10800), (3179602800, 7200), (3194726400, 10800), (3209842800, 7200),
            (3226176000, 10800), (3240687600, 7200), (3244320000, 10800), (3244921200, 7200),
            (3257625600, 10800), (3271532400, 7200), (3275164800, 10800), (3276370800, 7200),
            (3289075200, 10800), (3301772400, 7200), (3306009600, 10800), (3307820400, 7200),
            (3321129600, 10800), (3332617200, 7200), (3336249600, 10800), (3339270000, 7200),
            (3352579200, 10800), (3362857200, 7200), (3367094400, 10800), (3370719600, 7200),
            (3384028800, 10800), (3393702000, 7200), (3397939200, 10800), (3402774000, 7200),
            (3415478400, 10800), (3424546800, 7200), (3428179200, 10800), (3434223600, 7200),
            (3446928000, 10800), (3454786800, 7200), (3459024000, 10800), (3465673200, 7200),
            (3478982400, 10800), (3485631600, 7200), (3489264000, 10800), (3497122800, 7200),
            (3510432000, 10800), (3516476400, 7200), (3520108800, 10800), (3528572400, 7200),
            (3541881600, 10800), (3546716400, 7200), (3550953600, 10800), (3560022000, 7200),
            (3573331200, 10800), (3577561200, 7200), (3581193600, 10800), (3592076400, 7200),
            (3604780800, 10800), (3607801200, 7200), (3612038400, 10800), (3623526000, 7200),
            (3636230400, 10800), (3638646000, 7200), (3642883200, 10800), (3654975600, 7200),
            (3668284800, 10800), (3669490800, 7200), (3673123200, 10800), (3686425200, 7200),
        }),
        ("canada/mountain", -25200, new (long, int)[]
        {
            (1741510800, -21600), (1762070400, -25200), (1772960400, -21600), (1793520000, -21600),
        }),
        ("canada/pacific", -28800, new (long, int)[]
        {
            (1741514400, -25200), (1762074000, -28800), (1772964000, -25200), (1793523600, -25200),
        }),
    };
    // END GENERATED TZ
}
