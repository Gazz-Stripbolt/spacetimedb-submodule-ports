# cron: calendar and interval jobs

Port of [`@spacetimedb/cron`](https://github.com/clockworklabs/SpacetimeDB/pull/5903) at
`ae25f1da24b1723e28acd0038e75e598f33952c3`, including its expression engine: cron-parser 5.5.0, re-implemented,
with luxon's DST arithmetic and a time-zone table generated from IANA tzdata.

**What matches TS:**
- **Schema:** the `cron_job` and `cron_run` tables, the `cron_reconcile` sweep and the public `cron_jobs`
  projection (`CronJobView`).
- **Behaviour:** invocation ids (`job:generation:sequence`), failure counting and `maxFailures` auto-disable,
  generations, catch-up, yearly checkpoints for sparse schedules, lost-trigger repair, history caps, and the
  volatile failure-recovery call after a reducer job rolls back.
- **Answers:** every next-occurrence answer matches the TS package: 43,632 reference cases from the real
  cron-parser, across 18 zones and DST edges.

## Rust

```rust
pub mod cron;   // copy rust/cron.rs

cron_jobs! {
    history_cap: 5,
    reconcile_every_seconds: 0,       // 0 = no periodic repair sweep
    jobs: {
        heartbeat => reducer,
        report(ReportArgs) => reducer,
        sync_feed => procedure,       // can make HTTP requests
    }
}

pub fn cron_run(ctx: &ReducerContext, job: &CronJobArgsValue, inv: &cron::CronInvocation) -> Result<(), String> { .. }
pub fn cron_run_procedure(ctx: &mut ProcedureContext, job: &CronJobArgsValue, inv: &cron::CronInvocation) -> Result<(), String> { .. }

// From any reducer:
cron::schedule(ctx, CronJobArgsValue::report(args), cron::cron("0 9 * * MON-FRI"),
    cron::ScheduleOpts { timezone: Some("America/New_York".into()), max_failures: Some(3) })?;
cron::schedule(ctx, CronJobArgsValue::heartbeat, cron::every(30), Default::default())?;
cron::unschedule(ctx, "heartbeat")?;

#[spacetimedb::view(accessor = cron_jobs, public)]
fn cron_jobs(ctx: &ViewContext) -> Vec<cron::CronJobView> { cron::public_jobs(ctx) }
```

## C#

```csharp
[SpacetimeDB.Type]
public partial record CronJobArgsValue : TaggedEnum<(Unit heartbeat, ReportArgs report, Unit sync_feed)>;

public static partial class Module
{
    public static partial CronSettings CronConfigure() => new(
        Jobs: [CronJobSpec.Reducer("heartbeat"), CronJobSpec.Reducer("report"), CronJobSpec.Procedure("sync_feed")]);
    public static partial void CronRunJob(ReducerContext ctx, CronJobArgsValue job, CronInvocation inv) { ... }
    public static partial void CronRunProcedureJob(ProcedureContext ctx, CronJobArgsValue job, CronInvocation inv) { ... }
}

CronScheduleJob(ctx, new CronJobArgsValue.report(args), CronScheduleSpec.Cron("0 9 * * MON-FRI"),
    new CronScheduleOpts { Timezone = "America/New_York", MaxFailures = 3 });
CronUnscheduleJob(ctx, "heartbeat");
```

| TypeScript | Rust | C# |
|---|---|---|
| `cronTable({ name, args })` | a line in `cron_jobs!` | a `CronJobArgsValue` variant + `CronJobSpec` |
| `cron.schedule(ctx, job, spec, opts)` | `cron::schedule(ctx, args, spec, opts)` | `CronScheduleJob(ctx, args, spec, opts)` |
| `cron.unschedule(ctx, job)` | `cron::unschedule(ctx, name)` | `CronUnscheduleJob(ctx, name)` |
| `job.cronReducer(schema, handler)` | `crate::cron_run` | `CronRunJob` |
| `job.cronProcedure(schema, handler)` | `crate::cron_run_procedure` | `CronRunProcedureJob` |
| `cron.reconcileReducer(schema)` | built in (`cron_reconcile`), on when `reconcile_every_seconds > 0` | built in, `ReconcileEverySeconds > 0` |
| `cron.publicViews(schema).jobs` | `cron::public_jobs(view_ctx)` in your view | `CronPublicJobs(viewCtx)` in your view |
| `parseCronExpression`, `nextFireAfter`, `isValidTimezone` | `parse_cron_expression`, `next_fire_after`, `is_valid_timezone` | `CronParseExpression`, `CronNextFireAfter`, `CronIsValidTimezone` |

**Differences:**
- **Shared trigger tables.** TS generates a `<job>_fire` table and a `<job>_cron` function per job. Here, reducer
  jobs share `cron_fire` (scheduled reducer `cron_tick`) and procedure jobs share `cron_procedure_fire`
  (`cron_tick_procedure`), one row per job.
- **Time zones.** They come from a generated table: tzdata 2026c, `python3 tools/gen-tz.py` to refresh. It holds
  each zone's current rule, plus explicit transitions from 2025 for the zones that need them. Results are exact for
  2025 onwards; earlier dates use the current rule, which scheduling never needs. Names are case-insensitive, and
  `UTC` and `±HH:MM` work.
- **Job arguments are typed.** `cron.missing_args` and `cron.unexpected_args` can't happen.
- **Rust panics.** A panicking Rust handler aborts like an error but can't schedule the recovery call, so the
  failure is recorded later by reconciliation (`lost_fire`). Return `Err`.
- **Private tables only.** `publicTables` isn't supported: tables are private, so expose the public view instead.
- **Extra index.** `cron_job` has an index on `enabled`, because Rust and C# views can only read indexes.
- **Not ported:** the TS recovery test that kills the host mid-procedure (`recovery_probe`).

**Tests:**
- [`tests/cron-vectors.test.mjs`](../tests/cron-vectors.test.mjs) checks the expression engine:
  - parse acceptance for 91 expressions
  - 43,632 next-occurrence cases generated by the real cron-parser 5.5.0 ([`tools/gen-cron-vectors.mjs`](../tools/gen-cron-vectors.mjs))
  - 25,098 UTC offsets for every zone, including both sides of every transition from 2025 to 2031 ([`tools/gen-tz-vectors.py`](../tools/gen-tz-vectors.py))
- [`tests/cron.test.mjs`](../tests/cron.test.mjs) replays the upstream integration scenarios against the real
  scheduler: lost-trigger repair, the reconcile sweep, cadence, argument payloads, rollback and recovery,
  auto-disable, generations, procedure jobs, and unschedule.
