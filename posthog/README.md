# posthog: product analytics with a durable outbox, plus feature flags

Port of [`@spacetimedb/posthog`](https://github.com/clockworklabs/SpacetimeDB/pull/5907) at
`b1063b1876f4d7b8c723adb54b3d0f4d248b9eb0`.

**Schema (same as TS):** tables `posthog_config`, `posthog_admin_identity`, `posthog_outbox`, `posthog_flush_tick`
(scheduled), `posthog_delivery_stats` and `posthog_delivery_log`; views `posthog_outbox_admin` and
`posthog_delivery_log_admin`.
Behaviour: events queue in your transaction and a scheduled procedure sends them to `/batch` every 5 s (up to 10
batches of 100). Retries back off from 1 s doubling to 5 min, 4xx (except 408/429) fails at once, and the fifth
failure is final. Claims expire after 5 min, delivered/failed rows and logs are pruned after 30 days, and log
bodies are truncated to 2,048 characters.

## Rust

Copy [`rust/posthog.rs`](rust/posthog.rs) (dependency `serde_json` with `preserve_order`), then:

```rust
#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) { posthog::install(ctx); }

#[spacetimedb::reducer]
pub fn complete_order(ctx: &ReducerContext, order_id: String) {
    // … your change …
    posthog::enqueue_event_in_tx(ctx, posthog::EnqueueEventArgs {
        distinct_id: ctx.sender().to_hex().to_string(), event: "order_completed".into(),
        properties_json: Some(format!(r#"{{"orderId":"{order_id}"}}"#)),
        idempotency_key: Some(format!("order_completed:{order_id}")),
    });
}
```

## C#

Add [`csharp/Posthog.cs`](csharp/Posthog.cs), then:

```csharp
[Reducer(ReducerKind.Init)]
public static void Init(ReducerContext ctx) => PosthogInstall(ctx);

[Reducer]
public static void CompleteOrder(ReducerContext ctx, string orderId)
{
    // … your change …
    PosthogEnqueueEventInTx(ctx, new PosthogEnqueueEventArgs
    {
        DistinctId = ctx.Sender.ToString().ToLowerInvariant(), Event = "order_completed",
        PropertiesJson = $"{{\"orderId\":\"{orderId}\"}}", IdempotencyKey = $"order_completed:{orderId}",
    });
}
```

Then, as an admin, call `set_posthog_config(host, project_api_key)`, e.g. `https://us.i.posthog.com`.

| TypeScript | Rust | C# |
|---|---|---|
| `install(ctx)` | `posthog::install(ctx)` | `PosthogInstall(ctx)` |
| `enqueueEventInTx(ctx, args)` | `posthog::enqueue_event_in_tx(ctx, EnqueueEventArgs)` | `PosthogEnqueueEventInTx(ctx, PosthogEnqueueEventArgs)` |
| `setPosthogConfig`, `getPosthogConfigStatus` | `set_posthog_config`, `get_posthog_config_status` | `SetPosthogConfig`, `GetPosthogConfigStatus` |
| `addAdminIdentity`, `removeAdminIdentity` | `posthog_add_admin_identity`, `posthog_remove_admin_identity` | `PosthogAddAdminIdentity`, … |
| `enqueueEvent`, `captureNow`, `flushOutbox` | `posthog_enqueue_event`, `posthog_capture_now`, `posthog_flush_outbox` | `PosthogEnqueueEvent`, … |
| `requeueFailedEvents`, `clearAnalytics`, `getFeatureFlag` | `posthog_requeue_failed_events`, `posthog_clear_analytics`, `posthog_get_feature_flag` | `Posthog…` |
| `scheduledFlush` | `posthog_scheduled_flush` | `PosthogScheduledFlush` |
| `posthogOutboxAdmin`, `posthogDeliveryLogAdmin` | built in (views) | built in (views) |

**Differences:**
- **Names:** generic procedures and reducers get a `posthog_` prefix.
- **Results:** procedures return `Result<T, String>` instead of throwing.
- **Scheduler-only:** `posthog_scheduled_flush` rejects callers other than the scheduler, as in TS. It keeps the
  unit return, so a rejected call is a procedure error.
- **JSON:** event bodies are serialized like JS `JSON.stringify`, with integer-like keys first and properties in
  their original order.
- **C# names:** C# accessors are `Posthog…`, not `PostHog…`, so the canonical names stay `posthog_…` rather than
  `post_hog_…`. The row and result types keep the TS `PostHog…` spelling.

**Tests:** [`tests/posthog.test.mjs`](../tests/posthog.test.mjs) has 13 cases ported from the upstream suite: config,
admins, validation and rollback, idempotency, the batch body, backoff and retry limits, 408/429, truncation,
capture, feature flags, the admin views, retention, clearing, and the real scheduler. All run against a local mock
PostHog; the demo's `demo_pause_scheduler` / `demo_prune_history_at` hooks let tests drive delivery and time.
