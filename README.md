<div align="center">

# 🔧 SpacetimeDB Submodule Ports

**The official SpacetimeDB TypeScript submodules, as drop-in files for Rust and C# modules.**

> **Unofficial ports.** These are community ports of Clockwork Labs' `@spacetimedb/*` packages, not affiliated
> with or endorsed by Clockwork Labs. They are a stopgap until submodules come to Rust and C#.

Submodules are TypeScript-only today, but most SpacetimeDB modules are WASM modules written in Rust or C#. This repo
ports each package to one Rust file and one C# file with **the same tables, reducers, error codes and behaviour**,
pinned to the upstream commit and kept in sync as the packages evolve.

[![CI](https://github.com/Gazz-Stripbolt/spacetimedb-submodule-ports/actions/workflows/ci.yml/badge.svg)](https://github.com/Gazz-Stripbolt/spacetimedb-submodule-ports/actions/workflows/ci.yml)
![SpacetimeDB 2.11](https://img.shields.io/badge/SpacetimeDB-2.11-e8730c)
![Rust](https://img.shields.io/badge/Rust-drop--in-b7410e)
![C#](https://img.shields.io/badge/C%23-drop--in-512bd4)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

</div>

---

## Where to go

| Package | What it does | Rust | C# | Ported from |
|---|---|:-:|:-:|---|
| [**crypto**](crypto) | SHA-256, HMAC, hex/base64, Stripe / Svix / GitHub webhook signatures | ✅ | ✅ | [#5903](https://github.com/clockworklabs/SpacetimeDB/pull/5903) `ae25f1d` |
| [**retry**](retry) | Background tasks with exponential backoff, attempt history, admins | ✅ | ✅ | [#5903](https://github.com/clockworklabs/SpacetimeDB/pull/5903) `ae25f1d` |
| [**rate-limit**](rate-limit) | Fixed-window limits per scope and actor, expiry sweep, admins | ✅ | ✅ | [#5903](https://github.com/clockworklabs/SpacetimeDB/pull/5903) `ae25f1d` |
| [**cron**](cron) | Calendar (cron expressions, time zones) and interval jobs | ✅ | ✅ | [#5903](https://github.com/clockworklabs/SpacetimeDB/pull/5903) `ae25f1d` |
| resend | Email via Resend | 🚧 | 🚧 | [#5903](https://github.com/clockworklabs/SpacetimeDB/pull/5903) |
| [**presence**](presence) | Who is here: leases that expire, heartbeat, online view, sweep | ✅ | ✅ | [#5908](https://github.com/clockworklabs/SpacetimeDB/pull/5908) `f6ed7dc` |
| [**api-keys**](api-keys) | Hashed server-to-server keys, scopes, rotation, revocation, usage log | ✅ | ✅ | [#5908](https://github.com/clockworklabs/SpacetimeDB/pull/5908) `f6ed7dc` |
| [**grid**](grid) | Square/hex maps, units, obstacles, A* paths, movement ranges | ✅ | ✅ | [#5908](https://github.com/clockworklabs/SpacetimeDB/pull/5908) `f6ed7dc` |
| [**lobby**](lobby) | Matchmaking queues (unranked and ranked bands), rooms, seats, Elo | ✅ | ✅ | [#5908](https://github.com/clockworklabs/SpacetimeDB/pull/5908) `f6ed7dc` |
| auth | Password and OAuth login, sessions, profiles | 🚧 | 🚧 | [#5905](https://github.com/clockworklabs/SpacetimeDB/pull/5905) |
| files | Uploads, visibility, HTTP serving | 🚧 | 🚧 | [#5905](https://github.com/clockworklabs/SpacetimeDB/pull/5905) |
| agents | LLM agents, tool calls, embeddings | 🚧 | 🚧 | [#5905](https://github.com/clockworklabs/SpacetimeDB/pull/5905) |
| stripe | Customers, subscriptions, Checkout, webhooks | 🚧 | 🚧 | [#5906](https://github.com/clockworklabs/SpacetimeDB/pull/5906) |
| posthog | Server-side analytics and feature flags | 🚧 | 🚧 | [#5907](https://github.com/clockworklabs/SpacetimeDB/pull/5907) |

Also here: [**PORTING.md**](PORTING.md), the conventions every port follows and how to re-sync with upstream, and
[**docs/FINDINGS.md**](docs/FINDINGS.md), the SpacetimeDB quirks and bugs the ports ran into.

## How a port looks

One file per language, dropped into your module:

```rust
// Rust: src/lib.rs
#[path = "retry.rs"]          // or `pub mod retry;` with the file in src/
pub mod retry;

#[derive(spacetimedb::SpacetimeType, Clone)]
#[allow(non_camel_case_types)]
pub enum RetryArgs { send_receipt(SendReceiptArgs) }

pub fn retry_handle(ctx: &ReducerContext, args: &RetryArgs) -> retry::RetryResult { /* ... */ }
```

```csharp
// C#: add Retry.cs to the project
[SpacetimeDB.Type]
public partial record RetryArgs : TaggedEnum<(SendReceiptArgs send_receipt, Unit cleanup)>;

public static partial class Module
{
    public static partial RetryResult RetryHandle(ReducerContext ctx, RetryArgs args) { /* ... */ }
}
```

Each package README maps the TypeScript API to both ports and lists every difference.

## How they're tested

Both languages run **the same suite**, through SpacetimeDB's HTTP API against a demo module per language, using
the upstream package's own test cases and published vectors. CI also checks that the Rust and C# ports of each
package expose an **identical schema** ([`tests/schema-parity.mjs`](tests/schema-parity.mjs)).

| Package | Tests per language |
|---|---|
| crypto | NIST and RFC 6234 SHA-256, RFC 4231 HMAC, RFC 4648 base64 plus every malformed case, and Stripe / Svix / GitHub round trips (8 suites) |
| retry | Validation, admins, backoff timing, give-up, permanent errors, truncation, and the 1,000-row history cap (10) |
| rate-limit | Scopes, actors, costs, windows, peek, key collisions, sweep, admin reducers and view (8) |
| cron | **43,632** next-occurrence cases from the real cron-parser 5.5.0, **25,098** tzdata offsets, and 14 scheduler scenarios: repair, recovery, auto-disable, procedures… (18) |
| presence | Default TTLs, validation, key collisions, touch, upsert, remove, config, sweep batches, heartbeat, views, admins (12) |
| api-keys | Key derivation, hashing and normalized JSON **byte-identical to TS** (Node HMAC / SHA-256 / `JSON.stringify(JSON.parse(x))`), config, create, verify and its usage log, rotate, revoke, expiry, the 50-key cap, retention (14) |
| grid | Upstream pathfinding and procedure tests, **240** random maps matching the TS A* and Dijkstra exactly (paths, costs, expansions, order), modes, cells, entities, paths, ranges, cascades (13) |
| lobby | **300** ranked selections and **300** Elo updates from the TS code, room lifecycle, queues, ranked pools and draws, views, config and sweep (13) |

```bash
spacetime start                         # in another terminal
scripts/e2e.sh cron rust                # publish cron/demo/rust and run its suite
scripts/e2e.sh cron csharp
scripts/parity.sh cron                  # both ports, same schema
```

## Credits

The packages, their design and their tests are by [Clockwork Labs](https://spacetimedb.com/?referral=Lethalchip)
(Apache-2.0; see [NOTICE](NOTICE)). The ports are by **Tinker** ([@Gazz-Stripbolt](https://github.com/Gazz-Stripbolt)),
the resident gadgeteer for the [Pogly](https://pogly.gg) team: collaborative stream overlays, powered by SpacetimeDB.

More SpacetimeDB building blocks from this workshop: **[github.com/Gazz-Stripbolt](https://github.com/Gazz-Stripbolt)**.

🚀 **New to SpacetimeDB?** If you sign up through **[this referral link](https://spacetimedb.com/?referral=Lethalchip)**,
Pogly gets free recurring energy. Thank you!

## License

[Apache-2.0](LICENSE), like the packages it ports.
