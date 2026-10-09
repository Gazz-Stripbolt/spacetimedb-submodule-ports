# rate-limit: fixed-window rate limits

Port of [`@spacetimedb/rate-limit`](https://github.com/clockworklabs/SpacetimeDB/pull/5903) at
`ae25f1da24b1723e28acd0038e75e598f33952c3`.

**Schema:** tables `rate_limit_bucket`, `rate_limit_admin_identity`, `rate_limit_config` (public) and
`rate_limit_sweep_tick` (every 30 s); reducers `add_rate_limit_admin`, `remove_rate_limit_admin`,
`rate_limit_update_config`, `rate_limit_reset_buckets` and the scheduled `rate_limit_sweep`; procedure
`rate_limit_run_sweep`; view `admin_rate_limit_buckets`.
- **Renamed** to carry the package prefix: TS's `update_config`, `reset_buckets` and `run_sweep`. In TS they live
  under the submodule's namespace.
- **Bucket keys** are length-prefixed with UTF-16 lengths exactly like TS (`5:login9:ip:1.2.3.4`), so the same
  actor maps to the same row in every language.

```rust
const LOGIN: rate_limit::RateLimitPolicy = rate_limit::policy("auth.login", 5, 60);

let r = LOGIN.consume(ctx, &ctx.sender().to_hex(), 1)?;
if !r.allowed { return Err(format!("try again in {}s", r.retry_after_seconds)); }
```

```csharp
static readonly RateLimitPolicy Login = new("auth.login", Limit: 5, WindowSeconds: 60);

var r = Login.Consume(ctx, ctx.Sender.ToString());
if (!r.Allowed) throw new Exception($"try again in {r.RetryAfterSeconds}s");
```

| TypeScript | Rust | C# |
|---|---|---|
| `client({ scope, limit, windowSeconds })` | `const P: RateLimitPolicy = policy(scope, limit, window)` | `new RateLimitPolicy(scope, limit, window)` |
| `consume(tx, { key, cost })` | `P.consume(ctx, key, cost) -> Result<RateLimitResult>` | `P.Consume(ctx, key, cost)` / `P.Consume(tx.Db, tx.Timestamp, ...)` |
| `peek(db, key, now?)` | `P.peek(ctx_or_view_ctx, key, Some(now))` | `P.Peek(ctx, key, now)` / `P.Peek(viewCtx, key)` |
| `install(ctx)` | `rate_limit::install(ctx)` | `RateLimitInstall(ctx)` |
| `requireAdmin` / `isAdmin` | `rate_limit::require_admin` / `is_admin` | `RateLimitRequireAdmin` / `RateLimitIsAdmin` |

**Differences:**
- **Policy errors on use:** TS rejects a second `client()` for a scope while the module loads. A drop-in has no
  load-time hook, so declaring a scope with two different limits or windows fails on use
  (`rate_limit.duplicate_scope`), as do the other policy errors.
- **Scheduler-only:** the sweep reducer rejects callers other than the scheduler.

**Tests:** the upstream cases (per-scope and per-actor limits, cost validation, windows, peek, key collisions and
the sweep) plus the admin reducers and view, in [`tests/rate-limit.test.mjs`](../tests/rate-limit.test.mjs).
