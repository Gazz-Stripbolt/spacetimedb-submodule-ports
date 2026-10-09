# retry: retry background tasks with exponential backoff

Port of [`@spacetimedb/retry`](https://github.com/clockworklabs/SpacetimeDB/pull/5903) at
`ae25f1da24b1723e28acd0038e75e598f33952c3`.

**Schema (same as TS):** tables `retry_task` (scheduled), `retry_history` and `retry_admin_identity`; reducers
`submit_retry_task`, `add_retry_admin_identity`, `remove_retry_admin_identity` and the scheduled `retry_fire`; views
`retry_tasks_admin` and `retry_history_admin`.
Behaviour: delays are `backoff_secs × 2^attempt`, at most 10 attempts, history capped at the latest 1,000, and
errors truncated to 2,048 characters.

## Rust

Copy [`rust/retry.rs`](rust/retry.rs). Your crate root defines the task arguments and one handler:

```rust
#[derive(SpacetimeType, Clone)]
#[allow(non_camel_case_types)]
pub enum RetryArgs { send_receipt(SendReceiptArgs), cleanup }

pub fn retry_handle(ctx: &ReducerContext, args: &RetryArgs) -> retry::RetryResult {
    match args {
        RetryArgs::send_receipt(a) => if send(ctx, a) { retry::ok() } else { retry::failed("smtp down") },
        RetryArgs::cleanup => retry::ok(),
    }
}

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) { retry::install(ctx); }
```

## C#

Add [`csharp/Retry.cs`](csharp/Retry.cs), then:

```csharp
[SpacetimeDB.Type]
public partial record RetryArgs : TaggedEnum<(SendReceiptArgs send_receipt, Unit cleanup)>;

public static partial class Module
{
    public static partial RetryResult RetryHandle(ReducerContext ctx, RetryArgs args) => args switch
    {
        RetryArgs.send_receipt(var a) => Send(ctx, a) ? RetryResult.Ok() : RetryResult.Failed("smtp down"),
        _ => RetryResult.Ok(),
    };

    [Reducer(ReducerKind.Init)]
    public static void Init(ReducerContext ctx) => RetryInstall(ctx);
}
```

| TypeScript | Rust | C# |
|---|---|---|
| `client({ tasks })` | `enum RetryArgs` + `fn retry_handle` in the crate root | `RetryArgs` TaggedEnum + `partial RetryHandle` |
| `retryOk()` / `retryFailed(e)` | `retry::ok()` / `retry::failed(e)` | `RetryResult.Ok()` / `RetryResult.Failed(e)` |
| throw `SenderError` (give up) | `retry::permanent(e)` | `RetryResult.Permanent(e)` or throw `RetryPermanentException` |
| throw `Error` (retry) | (return `failed`) | throw any other exception |
| `install(ctx)` | `retry::install(ctx)` | `RetryInstall(ctx)` |
| `submit(ctx, task)` (no auth) | `retry::submit(ctx, name, args, max, backoff)` | `RetrySubmit(...)` |
| `requireAdmin(ctx)` | `retry::require_admin(ctx)?` | `RetryRequireAdmin(ctx)` |
| `retryReducer(schema, handlers)` | built in: `retry_fire` | built in: `RetryFire` |
| `views.*`, `reducers.*` | built in | built in |

**Differences:**
- **Rust panics:** Rust can't catch a panic in wasm, so a panicking handler aborts its attempt: no history and no
  retry. Return an error instead. (C#, like TS, keeps the writes a handler made before it threw.)
- **Scheduler-only:** `retry_fire` rejects callers other than the scheduler.
- **`run_attempt`:** `run_attempt` / `RetryRunAttempt` is public.

**Tests:** the upstream cases (validation, admins, backoff timing, give-up, permanent errors, error truncation and
the 1,000-row history cap) run against the real scheduler, in [`tests/retry.test.mjs`](../tests/retry.test.mjs).
