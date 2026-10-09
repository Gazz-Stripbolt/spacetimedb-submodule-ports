# api-keys: API keys for server-to-server callers

Port of [`@spacetimedb/api-keys`](https://github.com/clockworklabs/SpacetimeDB/pull/5908) at
`f6ed7dcd1c8f853c97fc7e9f126606fca7adadac`. It needs the [crypto](../crypto) port next to it (Rust:
`crate::crypto`; C#: `Crypto.cs` in the same project).

**Schema:** no renames; every name already carries the package.
- **Tables:** `api_key`, `api_key_admin_identity`, `api_key_config`, `api_key_usage` (all private) and
  `api_key_sweep_tick` (every 60 s).
- **Procedures:** `create_api_key`, `create_api_key_for_subject`, `rotate_api_key`.
- **Reducers:** `revoke_api_key`, `revoke_api_key_for_subject`, `set_api_keys_config`, `add_api_keys_admin`,
  `remove_api_keys_admin`, plus the scheduled `api_keys_sweep`.
- **Views:** `my_api_keys`, `api_keys_admin`, `api_key_usage_admin`.

**Keys match TS exactly:**
- **Format:** `stdb_live_<64 hex>`, with a 16-hex lookup prefix and the SHA-256 stored.
- **Derivation:** HMAC-SHA256 of the operator secret over `spacetimedb-api-keys secret {counter} {micros}`, so the
  same secret, counter and timestamp give the same key in every language.
- **Normalized JSON:** `scopes_json`/`metadata_json` are byte-identical to TS's `JSON.stringify(JSON.parse(x))`.
  That covers key order with integer-like keys first, number formatting, and string escapes.

```rust
// once, as an admin: set_api_keys_config(Some("<32+ char secret from outside the module>"), None)
let r = ctx.try_with_tx(|tx| api_keys::verify_api_key(tx, &api_keys::VerifyApiKeyArgs {
    key: key.clone(),
    required_scope: Some("files:write".into()),
    action: None,
}))?;
if !r.allowed { return Err(r.reason); }   // invalid_key, unknown_key, revoked, expired, scope_denied
```

```csharp
var r = ctx.WithTx(tx => ApiKeysVerify(ApiKeysTx.Of(ctx, tx), new VerifyApiKeyArgs(key) { RequiredScope = "files:write" }));
if (!r.Allowed) return Result<string, string>.Err(r.Reason);
```

| TypeScript | Rust | C# |
|---|---|---|
| `install(ctx)` | `api_keys::install(ctx)` | `ApiKeysInstall(ctx)` |
| `verifyApiKey(tx, args)` | `verify_api_key(ctx, &VerifyApiKeyArgs)` | `ApiKeysVerify(ApiKeysTx, VerifyApiKeyArgs)` |
| `createApiKeyInTx(tx, args)` | `create_api_key_in_tx(ctx, &CreateApiKeyArgs)` | `ApiKeysCreateInTx(tx, CreateApiKeyArgs)` |
| `rotateApiKeyInTx` / `revokeApiKeyInTx` | `rotate_api_key_in_tx` / `revoke_api_key_in_tx` | `ApiKeysRotateInTx` / `ApiKeysRevokeInTx` |
| `hasScope`, `hashApiKey`, `deriveKeySecret`, `formatApiKey`, `extractLookupPrefix` | same names, snake_case | `ApiKeysHasScope`, `ApiKeysHash`, `ApiKeysDeriveSecret`, `ApiKeysFormat`, `ApiKeysExtractLookupPrefix` |

**Differences:**
- **Procedure results:** procedures return `Result<T, String>`. TS's throw a `SenderError`.
- **C# context:** C# helpers take an `ApiKeysTx` (db, timestamp, sender, UUID source) built with `ApiKeysTx.Of(ctx)` or
  `ApiKeysTx.Of(procCtx, tx)`, because a procedure's transaction context can't mint UUIDs.
- **Scheduler-only:** the usage sweep rejects callers other than the scheduler. It's also available as
  `run_usage_sweep` / `ApiKeysRunUsageSweep`.
- **JSON nesting:** JSON nested deeper than 512 levels is rejected; TS has no limit.

**Tests:** the upstream cases (derivation, format, lookup prefix, hashing, scope matching) plus every procedure,
reducer and view, in [`tests/api-keys.test.mjs`](../tests/api-keys.test.mjs). Keys, hashes and normalized JSON are
compared against Node's own HMAC, SHA-256 and `JSON.stringify(JSON.parse(x))`.
