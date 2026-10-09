// Retry.cs: retry failed background tasks with exponential backoff, for C# modules.
//
// Unofficial C# port of @spacetimedb/retry (SpacetimeDB PR #5903, commit
// ae25f1da24b1723e28acd0038e75e598f33952c3, package spacetime-retry-ts). Licensed
// Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
// is by Tinker (Gazz-Stripbolt).
//
// Same tables (retry_task, retry_history, retry_admin_identity), reducers (submit_retry_task,
// add_retry_admin_identity, remove_retry_admin_identity), views (retry_tasks_admin,
// retry_history_admin), error codes and backoff as the TS package. The scheduled reducer is
// retry_fire (the TS example's name).
//
// Your module provides the task arguments and one handler:
//
//     [SpacetimeDB.Type]
//     public partial record RetryArgs : SpacetimeDB.TaggedEnum<(SendReceiptArgs SendReceipt, Unit Cleanup)>;
//
//     public static partial class Module
//     {
//         public static partial RetryResult RetryHandle(ReducerContext ctx, RetryArgs args) => args switch
//         {
//             RetryArgs.SendReceipt(var a) => Send(ctx, a) ? RetryResult.Ok() : RetryResult.Failed("smtp down"),
//             _ => RetryResult.Ok(),
//         };
//
//         [Reducer(ReducerKind.Init)]
//         public static void Init(ReducerContext ctx) => RetryInstall(ctx);
//     }
//
// Changes from the TypeScript original:
// - A thrown RetryPermanentException gives up immediately (the TS SenderError). Any other
//   exception counts as a failed attempt. As in TS, writes the handler made before throwing
//   are kept.
// - retry_fire rejects calls that don't come from the scheduler.
// - RetryRunAttempt is public, for hosts that want to drive an attempt themselves.

#pragma warning disable STDB_UNSTABLE
#nullable enable

using SpacetimeDB;

/// <summary>An attempt's outcome. Return Ok, Failed (retry after the backoff) or Permanent (give up).</summary>
public readonly record struct RetryResult(bool Succeeded, string? Error, bool IsPermanent)
{
    public static RetryResult Ok() => new(true, null, false);
    public static RetryResult Failed(string error) => new(false, error, false);
    public static RetryResult Permanent(string error) => new(false, error, true);
}

/// <summary>Throw from a handler to give up on the task immediately (the TS SenderError).</summary>
public sealed class RetryPermanentException(string message) : Exception(message);

[SpacetimeDB.Type]
public enum RetryHistoryStatus
{
    Ok,
    Failed,
    GaveUp,
}

public static partial class Module
{
    public const byte RetryMaxAttempts = 10;
    public const uint RetryMaxBackoffSeconds = 3600;
    public const int RetryMaxTaskNameLength = 128;
    public const int RetryMaxErrorLength = 2048;
    public const ulong RetryMaxRows = 1000;

    /// <summary>The retry.* codes reducers fail with.</summary>
    public static class RetryErrors
    {
        public const string NotAuthorized = "retry.not_authorized";
        public const string InvalidTaskName = "retry.invalid_task_name";
        public const string InvalidMaxAttempts = "retry.invalid_max_attempts";
        public const string InvalidBackoffSeconds = "retry.invalid_backoff_seconds";
        /// <summary>Followed by ':' and the task name.</summary>
        public const string TaskAlreadyExists = "retry.task_already_exists";
        public const string CannotRemoveLastAdmin = "retry.cannot_remove_last_admin";
    }

    /// <summary>Implement in your module: run one attempt of a task.</summary>
    public static partial RetryResult RetryHandle(ReducerContext ctx, RetryArgs args);

    [SpacetimeDB.Table(Accessor = "RetryTask", Scheduled = nameof(RetryFire), ScheduledAt = nameof(RetryTask.ScheduledAt))]
    public partial struct RetryTask
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong ScheduledId;
        public ScheduleAt ScheduledAt;
        [SpacetimeDB.Index.BTree]
        public string Name;
        public RetryArgs Args;
        public byte Attempt;
        public byte MaxAttempts;
        public uint BackoffSecs;
    }

    [SpacetimeDB.Table(Accessor = "RetryHistory")]
    public partial struct RetryHistory
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        public string TaskName;
        public byte Attempt;
        public RetryHistoryStatus Status;
        public string? Error;
        [SpacetimeDB.Index.BTree]
        public Timestamp RanAt;
    }

    [SpacetimeDB.Table(Accessor = "RetryAdminIdentity")]
    public partial struct RetryAdminIdentity
    {
        [SpacetimeDB.PrimaryKey]
        public Identity Identity;
        public Timestamp AddedAt;
    }

    // -----------------------------------------------------------------------
    // Library API
    // -----------------------------------------------------------------------

    /// <summary>Call from your Init reducer: makes the publishing identity the first admin.</summary>
    public static void RetryInstall(ReducerContext ctx) => RetryAddAdmin(ctx, ctx.Sender);

    public static bool RetryIsAdmin(ReducerContext ctx, Identity who) => ctx.Db.RetryAdminIdentity.Identity.Find(who) is not null;

    /// <summary>Throws retry.not_authorized unless the caller is a retry admin.</summary>
    public static void RetryRequireAdmin(ReducerContext ctx)
    {
        if (!RetryIsAdmin(ctx, ctx.Sender)) throw new Exception(RetryErrors.NotAuthorized);
    }

    static void RetryAddAdmin(ReducerContext ctx, Identity identity)
    {
        if (ctx.Db.RetryAdminIdentity.Identity.Find(identity) is null)
        {
            ctx.Db.RetryAdminIdentity.Insert(new RetryAdminIdentity { Identity = identity, AddedAt = ctx.Timestamp });
        }
    }

    /// <summary>
    /// Validate a task and schedule its first attempt now. No authorization check: call it
    /// from your own reducers after you've authorized the caller.
    /// </summary>
    public static void RetrySubmit(ReducerContext ctx, string name, RetryArgs args, byte maxAttempts, uint backoffSecs)
    {
        if (name.Length == 0 || name.Length > RetryMaxTaskNameLength) throw new Exception(RetryErrors.InvalidTaskName);
        if (maxAttempts < 1 || maxAttempts > RetryMaxAttempts) throw new Exception(RetryErrors.InvalidMaxAttempts);
        if (backoffSecs < 1 || backoffSecs > RetryMaxBackoffSeconds) throw new Exception(RetryErrors.InvalidBackoffSeconds);
        if (ctx.Db.RetryTask.Name.Filter(name).Any()) throw new Exception($"{RetryErrors.TaskAlreadyExists}:{name}");
        ctx.Db.RetryTask.Insert(new RetryTask
        {
            ScheduledId = 0,
            ScheduledAt = new ScheduleAt.Time(ctx.Timestamp),
            Name = name,
            Args = args,
            Attempt = 0,
            MaxAttempts = maxAttempts,
            BackoffSecs = backoffSecs,
        });
    }

    /// <summary>
    /// Run one attempt of a task in the current transaction: call the handler, record history,
    /// and schedule the next attempt if it failed and attempts remain. retry_fire calls this.
    /// </summary>
    public static void RetryRunAttempt(ReducerContext ctx, RetryTask task)
    {
        RetryResult result;
        try
        {
            result = RetryHandle(ctx, task.Args);
        }
        catch (RetryPermanentException e)
        {
            result = RetryResult.Permanent(e.Message);
        }
        catch (Exception e)
        {
            // Like TS: the handler's writes before the throw are kept.
            result = RetryResult.Failed(e.Message);
        }

        var isLast = !result.Succeeded && (result.IsPermanent || task.Attempt + 1 >= task.MaxAttempts);
        if (ctx.Db.RetryHistory.Count >= RetryMaxRows && RetryOldestHistory(ctx) is { } oldest)
        {
            ctx.Db.RetryHistory.Id.Delete(oldest);
        }
        var error = result.Error;
        ctx.Db.RetryHistory.Insert(new RetryHistory
        {
            Id = 0,
            TaskName = task.Name,
            Attempt = task.Attempt,
            Status = result.Succeeded ? RetryHistoryStatus.Ok : isLast ? RetryHistoryStatus.GaveUp : RetryHistoryStatus.Failed,
            Error = result.Succeeded ? null : error!.Length <= RetryMaxErrorLength ? error : error[..RetryMaxErrorLength],
            RanAt = ctx.Timestamp,
        });
        if (result.Succeeded || isLast) return;

        var delay = (long)task.BackoffSecs * (1L << task.Attempt) * 1_000_000;
        ctx.Db.RetryTask.Insert(task with
        {
            ScheduledId = 0,
            ScheduledAt = new ScheduleAt.Time(ctx.Timestamp + new TimeDuration(delay)),
            Attempt = (byte)(task.Attempt + 1),
        });
    }

    static readonly Timestamp RetryTimestampMin = new(long.MinValue);
    static readonly Timestamp RetryTimestampMax = new(long.MaxValue);

    /// <summary>
    /// The oldest history row by RanAt. Within a transaction a btree range scan yields the
    /// rows that transaction inserted first, then the committed rows, each part in key order
    /// (SpacetimeDB 2.11), so the minimum is the first row or the first where the key drops.
    /// </summary>
    static ulong? RetryOldestHistory(ReducerContext ctx)
    {
        RetryHistory? first = null;
        Timestamp prev = default;
        foreach (var row in ctx.Db.RetryHistory.RanAt.Filter(new Bound<Timestamp>(RetryTimestampMin, RetryTimestampMax)))
        {
            if (first is not { } f)
            {
                first = row;
                prev = row.RanAt;
                continue;
            }
            if (row.RanAt.MicrosecondsSinceUnixEpoch < prev.MicrosecondsSinceUnixEpoch)
            {
                return row.RanAt.MicrosecondsSinceUnixEpoch < f.RanAt.MicrosecondsSinceUnixEpoch ? row.Id : f.Id;
            }
            prev = row.RanAt;
        }
        return first?.Id;
    }

    // -----------------------------------------------------------------------
    // Reducers and views
    // -----------------------------------------------------------------------

    /// <summary>The scheduled reducer that runs attempts.</summary>
    [SpacetimeDB.Reducer]
    public static void RetryFire(ReducerContext ctx, RetryTask arg)
    {
        if (ctx.Sender != ctx.DatabaseIdentity) throw new Exception(RetryErrors.NotAuthorized);
        RetryRunAttempt(ctx, arg);
    }

    /// <summary>Submit a task (retry admins only).</summary>
    [SpacetimeDB.Reducer]
    public static void SubmitRetryTask(ReducerContext ctx, string name, RetryArgs args, byte maxAttempts, uint backoffSecs)
    {
        RetryRequireAdmin(ctx);
        RetrySubmit(ctx, name, args, maxAttempts, backoffSecs);
    }

    [SpacetimeDB.Reducer]
    public static void AddRetryAdminIdentity(ReducerContext ctx, Identity identity)
    {
        RetryRequireAdmin(ctx);
        RetryAddAdmin(ctx, identity);
    }

    [SpacetimeDB.Reducer]
    public static void RemoveRetryAdminIdentity(ReducerContext ctx, Identity identity)
    {
        RetryRequireAdmin(ctx);
        if (ctx.Db.RetryAdminIdentity.Identity.Find(identity) is null) return;
        if (ctx.Db.RetryAdminIdentity.Count <= 1) throw new Exception(RetryErrors.CannotRemoveLastAdmin);
        ctx.Db.RetryAdminIdentity.Identity.Delete(identity);
    }

    /// <summary>
    /// Views can't scan a table in 2.11, only read indexes, so this walks the whole Name
    /// index: from "" to past any valid name (names are at most 128 characters).
    /// </summary>
    static readonly Bound<string> RetryAllNames = new("", string.Concat(Enumerable.Repeat("\U0010FFFF", RetryMaxTaskNameLength + 1)));

    /// <summary>The latest pending tasks, newest first (retry admins only; others get nothing).</summary>
    [SpacetimeDB.View(Accessor = "RetryTasksAdmin", Public = true)]
    public static List<RetryTask> RetryTasksAdmin(ViewContext ctx)
    {
        if (ctx.Db.RetryAdminIdentity.Identity.Find(ctx.Sender) is null) return [];
        return ctx.Db.RetryTask.Name.Filter(RetryAllNames)
            .OrderByDescending(r => r.ScheduledId)
            .Take((int)RetryMaxRows)
            .ToList();
    }

    /// <summary>Retained attempt history, newest first (retry admins only).</summary>
    [SpacetimeDB.View(Accessor = "RetryHistoryAdmin", Public = true)]
    public static List<RetryHistory> RetryHistoryAdmin(ViewContext ctx)
    {
        if (ctx.Db.RetryAdminIdentity.Identity.Find(ctx.Sender) is null) return [];
        return ctx.Db.RetryHistory.RanAt.Filter(new Bound<Timestamp>(RetryTimestampMin, RetryTimestampMax))
            .OrderByDescending(r => r.RanAt.MicrosecondsSinceUnixEpoch)
            .ThenByDescending(r => r.Id)
            .ToList();
    }
}
