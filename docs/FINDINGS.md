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

## Case conversion details that affect ports

- **Digits start a new word.** `crypto_sha256` → `crypto_sha_256`, `base64` → `base_64`.
- **Enum variants become camelCase.** `long_error` → `longError`, and `GaveUp` → `gaveUp`, in every language.
- **Function parameter names aren't canonicalized.** Rust says `max_failures` and C# `maxFailures`. Only order and
  types matter on the wire, so the parity check normalizes them.
- **Identifiers can't share a name across kinds.** A C# table struct and a method in `Module` can't share one, and
  neither can a Rust table accessor and a reducer. That's why the cron hooks are `CronRunJob`, not `CronRun`.

## Other behaviour worth knowing

- **Scheduled reducers aren't reachable over HTTP:** `POST /v1/database/<db>/call/<scheduled reducer>` answers
  404 "No such procedure" in 2.11. The ports still check the caller in every scheduled function, as TS cron does,
  in case another transport allows it. (Upstream retry and rate-limit don't check.)
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
