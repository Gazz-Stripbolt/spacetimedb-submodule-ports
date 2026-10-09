# Findings

What porting the official TypeScript submodules to Rust and C# turned up, on SpacetimeDB 2.11.

## Bug: the generated C# `VolatileNonatomicScheduleImmediate<Reducer>` helper does nothing

For every reducer, the C# codegen emits `VolatileNonatomicScheduleImmediate<Name>(args)`
(`crates/bindings-csharp/Codegen/Module.cs`, ~L1607). It passes `nameof(<method>)`, the **C# method name**
(`CronTick`), to the host. Under the default snake_case policy the reducer's canonical name is `cron_tick`, so the
host never finds it and the call silently does nothing. We confirmed it with the cron recovery test:
- **Generated helper:** failures were only ever noticed later, by reconciliation, as `lost_fire`.
- **Canonical name (`SpacetimeDB.Internal.IReducer.VolatileNonatomicScheduleImmediate("cron_tick", …)`):**
  recovery worked.

`Cron.cs` uses the canonical name. Rust's `volatile_nonatomic_schedule_immediate!` works.

## Views can't scan tables in Rust or C#

TS views call `ctx.db.table.iter()`. In Rust and C#, a view's table handle only has `count` and its index
accessors, with no `iter()`, so a view can only read through an index. The ports walk an index over its full range
(`name().filter(""..)`, `ran_at().filter(Timestamp::MIN..)`). `cron_job` gained an index on `enabled` for this.

## Range scans inside a transaction aren't globally ordered

A btree range scan run inside a transaction yields the rows **that transaction inserted first**, then the
committed rows, each part in key order. (The TS retry code has a comment about this.) "The first row of
`ran_at().filter(..)` is the oldest" is therefore wrong once a transaction has inserted rows. `retry` finds the
true minimum by taking either the first row or the first row where the key goes down again.

**It reaches upstream's own logic, too:** `@spacetimedb/lobby` picks matches from `by_pool_status_created_at` in
the transaction that inserted the newest ticket. The newcomer therefore comes first: it takes seat 0, and it's the
first anchor ranked matching tries, although the code's comments say "oldest first". The lobby port keeps the
engine's order, so it behaves like TS on the same engine, and documents it in [lobby/README.md](../lobby/README.md).

## Case conversion details that affect ports

- **Digits start a new word.** `crypto_sha256` → `crypto_sha_256`, `base64` → `base_64`.
- **Enum variants become camelCase.** `long_error` → `longError`, and `GaveUp` → `gaveUp`, in every language.
- **Function parameter names aren't canonicalized.** Rust says `max_failures` and C# `maxFailures`. Only order and
  types matter on the wire, so the parity check normalizes them.
- **Identifiers can't share a name across kinds.** A C# table struct and a method in `Module` can't share one, and
  neither can a Rust table accessor and a reducer. That's why the cron hooks are `CronRunJob`, not `CronRun`.

## Other behaviour worth knowing

- **Only the database owner can call scheduled reducers.** For any other identity,
  `POST /v1/database/<db>/call/<scheduled reducer>` answers 404 "No such procedure" in 2.11. The owner's calls
  (`spacetime call`, or HTTP with the owner's token) do reach the reducer, and so would a container running as the
  database. So the ports check the caller in every scheduled function, as TS cron does. (Upstream retry and
  rate-limit don't check.)
- **Result-returning procedures** come back over HTTP as `[0, value]` / `[1, error]` with status 200, not as an
  HTTP error.
- **SQL over a view doesn't keep the view's row order**, so "newest first" views can only be checked by contents.
- **No `System.Security.Cryptography` on wasi:** `crypto/csharp/Crypto.cs` carries a managed SHA-256 and HMAC.
- **C# procedures read the database identity** from the static `ProcedureContextBase.Identity`; the instance form
  doesn't compile.

## cron-parser behaviour the ports reproduce

The cron port re-implements cron-parser 5.5.0's search loop and luxon's date arithmetic rather than a "clean"
algorithm, because users moving between languages should get the same times. That includes some surprising DST
behaviour, all checked against the real library in `tests/vectors/cron.json`:

- **Fall-back (repeated hour):** minute- and second-level schedules fire through **both** passes of the repeated
  hour (`*/15 * * * *` fires at 01:00, 01:15 … in EDT and again in EST). A daily `30 1 * * *` fires once (the
  first 01:30), unless you search from inside the second pass, which returns the second 01:30.
- **Spring-forward (skipped hour):** a skipped time fires at the shifted local time: `30 2 * * *` fires at
  03:30 EDT.
- **Search limit:** cron-parser gives up after 10,000 search steps without throwing and returns wherever it
  stopped. The port does the same.
- **Hashed fields:** `H` resolves with cron-parser's seeded PRNG (FNV-1a + mulberry32, seed
  `spacetimedb-cron-submodule`), so `H * * * *` picks the same minute (35) everywhere.

## Time zones without tzdata

Neither Rust nor C# modules can read the host's zoneinfo. The ports embed a table generated from IANA tzdata 2026c
(`tools/gen-tz.py`): all 597 zone and link names, each mapped to its POSIX rule (94 distinct rules), plus explicit
transitions from 2025 for the zones whose future isn't a simple rule:
- Morocco and Western Sahara (Ramadan changes)
- Palestine
- British Columbia and Alberta, which move to permanent time in November 2026 according to 2026c

Offsets match tzdata (via Python's `zoneinfo`) at 25,098 instants, covering both sides of every transition from
2025 to 2031. The cost is about 25 KB of source per language.

## Porting gotchas (phase 2: presence, api-keys, grid, lobby)

- **Rust can't range-filter a `String` index column with a `String` bound.** `filter("".to_string()..)` doesn't
  compile (`RangeFrom<String>` isn't an `IndexScanRangeBoundsTerminator`), but `filter(""..)` with `&str` does. C#'s
  `Bound<string>` works.
- **A C# `[SpacetimeDB.Type]` and a reducer/procedure can't share a name.** A type `DemoAstar` next to a procedure
  `DemoAstar` breaks the generated `ModuleRegistration` ("The type name 'BSATN' does not exist in the type
  'ModuleRegistration.DemoAstar'").
- **JS semantics that need care in Rust and C#:**
  - **Rounding:** `Math.round` is `floor(x + 0.5)`. C#'s `Math.Round` rounds half to even.
  - **Sorting:** `Array.prototype.sort` is stable. C#'s `List.Sort` isn't, so the ports use `OrderBy`.
  - **Lengths:** string lengths are UTF-16 code units.
  - **Whitespace:** `trim()` and `\s` cover Unicode spaces and BOM, not just ASCII.
- **Byte-identical JSON:** api-keys stores `JSON.stringify(JSON.parse(input))`. The ports carry a small
  JSON.parse/stringify that reproduces JS exactly:
  - property order, with integer-like keys first, ascending
  - number formatting via ECMAScript `Number::toString` on the shortest round-trip digits
  - escaping, including lone surrogates
  - duplicate keys: the last value wins, at the first key's position
  The test suite compares it with Node on edge cases. Library serializers (`serde_json`, `System.Text.Json`) differ
  in several of these.
- **A* and Dijkstra parity** needs the same heap, not just the same algorithm: tie-breaking decides which of several
  equal-cost paths is returned and how many nodes are expanded. grid ports TS's binary heap step for step and
  matches it on 240 generated maps.

## Integrations (resend, stripe, posthog)

- **Bug: a raw-identifier field breaks the Rust schema.** `#[derive(SpacetimeType)]` names fields with
  `Ident::to_string()` (`crates/bindings-macro/src/sats.rs`, ~L63), so `pub r#type: Option<String>` becomes a column
  called `r#type`. Publishing then fails with "Local module schema inspection failed". Stripe's
  `ValidateStripePriceResult.type` is `price_type` in both ports. Fix: `ident.unraw()`.
- **Rust can't filter an index on an `Option` column.** `Option<String>` isn't a `FilterableValue`, so lookups like
  `listEmailsByUserId` scan the table. The ports still declare the indexes, for schema parity.
- **Module HTTP to loopback is blocked** by standalone's SSRF protection. Testing against local mocks needs a server
  built with `allow_loopback_http_for_tests` (`scripts/dev-server.sh`; CI caches it).
- **C# `NewUuidV7()`** exists on `ReducerContext`, `ProcedureContext` and `HandlerContext`, but not on the
  `WithTx` context, so procedures make their UUIDs before entering the transaction.
- **C# accessor casing:** `PostHogOutbox` would canonicalize to `post_hog_outbox`. Accessors are spelled `Posthog…`
  to get `posthog_outbox` as in TS.
- **`HttpMethod` is ambiguous** in C# modules with implicit usings (`System.Net.Http.HttpMethod`); alias it to
  `SpacetimeDB.HttpMethod`.
- **Build-time configuration:** a drop-in file can't read the environment at runtime, so tests point resend and
  stripe at mocks at build time: `option_env!` in Rust, and an optional `static partial void` hook in C# that the
  demo csproj generates.

## Accounts, files and agents (auth, files, agents)

- **C# static initializers across partial files run in an undefined order.** A static field in one file that reads a
  static from another (the auth demo's `DemoMock = AuthGoogle with { … }`) threw a `TypeInitializationException`
  (a `NullReferenceException` inside) when the module loaded. The ports use static properties or lazily built fields
  for anything that depends on another file's statics.
- **`dotnet build` of a 2.11 module fails without `EXPERIMENTAL_WASM_AOT=1`:** the generated `FFI.cs` marks `Main`
  `[UnmanagedCallersOnly]`, which the compiler rejects for an entry point ("CS8899"). `spacetime build`/`publish`
  set the variable, and so does CI's build step.
- **The HTTP API's request size limit:** a 4 MB `byte[]` argument, sent as JSON to `/call`, is over the limit. The
  files suite exercises the 4 MB file limit with an upload generated inside the module.
- **Procedures interleave with other calls while they wait on HTTP.** During an agents run (a procedure blocked on a
  provider), reducers and other procedures for the same database run normally. The agents thread lock, the
  `thread_busy` rejection and mid-run cancellation rely on this, and the suite tests them with a held mock response.
- **Scheduled reducer calls in the agents tests reached the reducer** (530 `agents.not_authorized` from
  `POST /call/agents_thread_lock_sweep`), most likely because that probe used the owner's token. Re-checked on
  2026-10-09: a fresh anonymous identity gets 404 for scheduled reducers, and the owner reaches them. That matches the
  note above: owner-only, not public.
- **Re-serialized JSON in agents:** TS stringifies parsed provider values (Anthropic tool inputs) and host values
  (response formats). Rust (`serde_json` with `preserve_order`) and C# (a small ordered tree over `JsonDocument`) only
  need JS's output rules for that: integer-like keys first, and `Number::toString` formatting. Lone-surrogate escapes
  still don't survive parsing in either.
