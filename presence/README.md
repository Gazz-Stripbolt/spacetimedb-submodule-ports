# presence: who is here, with leases that expire

Port of [`@spacetimedb/presence`](https://github.com/clockworklabs/SpacetimeDB/pull/5908) at
`f6ed7dcd1c8f853c97fc7e9f126606fca7adadac`.

**Schema:**
- **Tables:** `presence_entry`, `presence_config`, `presence_admin_identity` (all private) and
  `presence_sweep_tick` (every 10 s).
- **Reducers:** `clear_presence`, `add_presence_admin`, `remove_presence_admin`, `presence_update_config`, plus the
  scheduled `presence_sweep`.
- **Procedures:** `presence_heartbeat` and `presence_run_sweep`.
- **Views:** `presence_online` (the `presence.global` scope) and `presence_entries_admin`.
- **Renamed** to carry the package prefix: TS's `heartbeat`, `run_sweep` and `update_config`. In TS they live
  under the submodule's namespace.
- **Keys** are trimmed and length-prefixed with UTF-16 lengths exactly like TS (`6:room:15:alice`), so the same
  scope and subject map to the same row in every language.

```rust
presence::upsert_presence(ctx, &presence::PresenceUpsertOpts {
    scope: format!("room:{room}"),
    subject: ctx.sender().to_hex().to_string(),
    status: Some("typing".into()),
    ..Default::default()
})?;
presence::touch_presence(ctx, &scope, &subject, None)?;   // extend the lease, keep the metadata
presence::remove_presence(ctx, &scope, &subject)?;
```

```csharp
PresenceUpsert(ctx, new PresenceUpsertOpts($"room:{room}", ctx.Sender.ToString()) { Status = "typing" });
PresenceTouch(ctx, scope, subject);
PresenceRemove(ctx, scope, subject);
// from a procedure: PresenceUpsert(tx.Db, tx.Timestamp, ...)
```

| TypeScript | Rust | C# |
|---|---|---|
| `install(ctx)` | `presence::install(ctx)` | `PresenceInstall(ctx)` |
| `upsertPresence(tx, opts)` | `upsert_presence(ctx, &PresenceUpsertOpts)` | `PresenceUpsert(ctx, new PresenceUpsertOpts(scope, subject) { ... })` |
| `touchPresence(tx, scope, subject, ttl?)` | `touch_presence(ctx, scope, subject, ttl)` | `PresenceTouch(ctx, scope, subject, ttl)` |
| `removePresence(tx, scope, subject)` | `remove_presence(ctx, scope, subject) -> Result<bool>` | `PresenceRemove(ctx, scope, subject)` |
| `buildPresenceKey(scope, subject)` | `build_presence_key` | `PresenceBuildKey` |
| `runPresenceSweep(tx, rows)` | `run_presence_sweep(ctx)` | `PresenceRunSweepNow(db, now)` |
| `GLOBAL_SCOPE` | `presence::GLOBAL_SCOPE` | `PresenceGlobalScope` |

**Differences:**
- **Procedure results:** procedures return `Result<T, String>`. TS's throw a `SenderError`.
- **Sweeping:** `run_presence_sweep` reads the `expires_at` index itself instead of taking an iterable.
- **Scheduler-only:** the sweep reducer rejects callers other than the scheduler.
- **Admin view:** `presence_entries_admin` walks the `expires_at` index (Rust and C# views can't scan tables).
- **Heartbeat subject:** the caller's identity as lowercase hex, like TS's `toHexString()`.

**Tests:** the upstream cases (default TTLs, validation, key collisions, touch keeping metadata, upsert updating
in place, remove, config, sweep batch) plus heartbeat, both views and the admin reducers, in
[`tests/presence.test.mjs`](../tests/presence.test.mjs).
