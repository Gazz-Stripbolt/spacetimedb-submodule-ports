# resend: send email through Resend and track delivery from its webhooks

Port of [`@spacetimedb/resend`](https://github.com/clockworklabs/SpacetimeDB/pull/5903) at
`ae25f1da24b1723e28acd0038e75e598f33952c3`. Needs [`crypto`](../crypto/) for the Svix webhook signature.

**Schema (same as TS):** private tables `resend_email`, `resend_delivery_event`, `resend_webhook_event`,
`resend_config` and `resend_admin_identity`.
Behaviour: same validation limits, error codes, status order (a late `email.sent` never overwrites `delivered`),
webhook dedupe and replay, and the same Svix check (5-minute tolerance, `v1,` signatures, rotated secrets).

## Rust

Copy [`rust/resend.rs`](rust/resend.rs) and [`../crypto/rust/crypto.rs`](../crypto/rust/crypto.rs) (as `crate::crypto`;
dependencies `sha2`, `hmac`, `serde_json` with `preserve_order`), then:

```rust
#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) { resend::install(ctx); }

#[router]
fn routes() -> Router { Router::new().post("/webhook/resend", resend::resend_webhook) }

// Your procedure decides who may send, then:
resend::send_email_request(ctx, resend::SendEmailArgs {
    to: vec![to], subject: "Welcome".into(), html: Some("<p>Hello.</p>".into()),
    tags_json: Some(r#"[{"name":"userId","value":"u_123"}]"#.into()), ..Default::default()
})?;
```

## C#

Add [`csharp/Resend.cs`](csharp/Resend.cs) and [`../crypto/csharp/Crypto.cs`](../crypto/csharp/Crypto.cs), then:

```csharp
[Reducer(ReducerKind.Init)]
public static void Init(ReducerContext ctx) => ResendInstall(ctx);

[SpacetimeDB.HttpRouter]
public static Router Routes() => Router.New().Post("/webhook/resend", Handlers.ResendWebhook);

// Your procedure decides who may send, then (throws on failure):
ResendSendEmailRequest(ctx, new ResendSendEmailArgs { To = [to], Subject = "Welcome", Html = "<p>Hello.</p>" });
```

| TypeScript | Rust | C# |
|---|---|---|
| `install(ctx)` | `resend::install(ctx)` | `ResendInstall(ctx)` |
| `sendEmailRequest(ctx, args)` | `resend::send_email_request(ctx, SendEmailArgs)` | `ResendSendEmailRequest(ctx, ResendSendEmailArgs)` |
| `makeResendWebhookHandler()` | `resend::resend_webhook` | `ResendWebhook` |
| `setResendConfig`, `getResendConfigStatus` | `set_resend_config`, `get_resend_config_status` | `SetResendConfig`, `GetResendConfigStatus` |
| `addAdminIdentity`, `removeAdminIdentity` | `resend_add_admin_identity`, `resend_remove_admin_identity` | `ResendAddAdminIdentity`, … |
| `sendEmail`, `cancelEmail`, `resendApiRequest`, `getEmail`, `getWebhookEvent` | `resend_send_email`, `resend_cancel_email`, … | `ResendSendEmail`, `ResendCancelEmail`, … |
| `listEmailsByUserId/OrgId/Status`, `listDeliveryEventsForEmail` | `resend_list_…` | `ResendList…` |
| `ingestResendWebhook`, `replayWebhookEvent` | `ingest_resend_webhook`, `resend_replay_webhook_event` | `IngestResendWebhook`, `ResendReplayWebhookEvent` |

**Differences:**
- **Names:** a drop-in shares your module's namespace, so generic procedures and reducers get a `resend_` prefix
  (table above) and types a `Resend` prefix.
- **Results:** procedures return `Result<T, String>` instead of throwing.
- **Scans:** `resend_list_emails_by_user_id` / `_by_org_id` scan the table. Rust can't filter an index on an
  `Option<String>` column, and C# does the same so the two behave alike.
- **Validation messages:** payload validation is hand-written (no valibot), so `Failed` webhook rows carry
  differently worded errors. Accept/reject decisions match.
- **Dates:** webhook `created_at` goes through a small ISO-8601 parser (date, `T`, time, optional fraction,
  `Z` or `±hh:mm`) instead of `Date.parse`. Other formats JS would accept count as invalid. `scheduled_at` is passed
  to Resend untouched, as in TS.
- **Test base URL:** build with `RESEND_API_BASE=http://127.0.0.1:4201` to send to a mock. In Rust this is
  `option_env!`; in C# the demo csproj generates the optional `ResendOverrideApiBase` partial. Production builds
  always use `https://api.resend.com`.

**Tests:** [`tests/resend.test.mjs`](../tests/resend.test.mjs) has 22 cases ported from the upstream suite: config
and admins, send validation, the request body and headers, idempotency, cancel, API passthrough, signed webhooks
(good, bad, stale, rotated), status ordering, dedupe and replay. All run against a local mock API
([`tests/mock-http.mjs`](../tests/mock-http.mjs)); nothing calls Resend.
