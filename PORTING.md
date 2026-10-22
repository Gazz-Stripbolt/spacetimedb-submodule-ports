# Porting conventions

How a `@spacetimedb/<name>` TypeScript package becomes a Rust and a C# drop-in here. Follow these so every
package feels the same and stays easy to update when upstream changes.

## Layout

```
<name>/
  rust/<name_with_underscores>.rs   one drop-in file: `#[path] mod` it or copy it to src/
  csharp/<PascalName>.cs            one drop-in file: add it to the module project
  README.md                         TS API → Rust API → C# API, differences, pinned commit
  demo/rust/                        a small module exercising the port (the TS example's shape)
  demo/csharp/                      the same module in C#
tests/<name>.test.mjs               one suite for both languages, over Spacetime's HTTP API
```

`scripts/e2e.sh <name> <rust|csharp>` publishes a demo and runs its suite; `scripts/parity.sh <name>` publishes
both and checks the schemas match (`tests/schema-parity.mjs`). Add the package to the CI matrices.

## Rules

1. **Pin and say so.** Each file's header names the package, the PR and commit it was ported from, the license
   (Apache-2.0), and a bullet list of changes from the original. Update the commit when you re-sync.
2. **Same schema as upstream.** Table, column, reducer, procedure and view names, column order and types follow
   the TS package. The canonical names come from Spacetime's default snake_case conversion, so write Rust
   `snake_case` and C# `PascalCase` (`Accessor = "RetryTask"` → `retry_task`). Check with `tests/schema-parity.mjs`:
   Rust and C# must match exactly (parameter names may differ in style; 2.11 doesn't canonicalize them).
3. **Submodule namespaces become prefixes only where needed.** A TS submodule's tables live under a namespace
   (`rate_limit.update_config`). A drop-in shares the host's namespace, so names that already carry the package
   (`rate_limit_bucket`) stay as they are and generic ones get the prefix (`update_config` →
   `rate_limit_update_config`). Note every rename in the README.
4. **Same error codes.** Reducers fail with the TS codes (`retry.not_authorized`, `cron.invalid_timezone:…`):
   `Err(String)` in Rust, a thrown `Exception` in C#. Procedures return `Result<T, String>`.
5. **Host hooks, not callbacks.** Where TS takes handler functions, Rust calls a function the host defines in the
   crate root (`crate::retry_handle`) and C# declares a `partial` method the host implements (`RetryHandle`).
   Host-defined argument types are an enum in Rust and a `TaggedEnum` in C#, with the TS task/job names as variants.
6. **Scheduled functions check their caller** (`ctx.sender() == ctx.database_identity()`), even where TS doesn't.
7. **No crypto, clocks or randomness from the platform.** C# on wasi has no `System.Security.Cryptography`
   (use `crypto/csharp/Crypto.cs`), modules have no wall clock (take `ctx.timestamp`), and module RNGs are
   seeded from the timestamp.
8. **Views can only read indexes** in Rust and C# (no table scans). Walk an index with a full range, adding an
   index if the TS table has none to use; note it.
9. **Test like upstream.** Reuse the package's own test cases and published vectors. Where behaviour depends on a
   library (cron-parser), generate expected answers from the real library (see `tools/gen-cron-vectors.mjs`).

## Updating to a new upstream commit

1. `git -C <SpacetimeDB clone> fetch origin <sha>` and diff the package between the old and new pins.
2. Port the changes to both files and their demos, update the pinned commit in the file headers, the package
   README and the root README table.
3. Re-run `scripts/e2e.sh` for both languages and `scripts/parity.sh`. For cron, regenerate the vectors if
   cron-parser's version changed, and the zone table (`python3 tools/gen-tz.py`) for a new tzdata release.
