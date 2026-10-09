# agents: AI assistants with tools

Port of [`@spacetimedb/agents`](https://github.com/clockworklabs/SpacetimeDB/pull/5905) at
`37ee2ad627568f95df8229647cff5a482d629c25`. The package itself stands alone. Rust needs `serde_json` (with
`preserve_order`) and `log`; C# needs nothing extra. The demo is the upstream example, so it also uses the
[auth](../auth) and [files](../files) ports (and, through them, [crypto](../crypto) and [rate-limit](../rate-limit)):
threads belong to the signed-in auth user, and image attachments are stored as owner-only files.

**Schema:**
- **Tables** (all private): `agent_config`, `agent_admin_identity`, `agent_override`, `agents_api_key`,
  `agents_thread`, `agents_message`, `agents_thread_lock`, `agents_message_embedding`, and
  `agents_thread_lock_sweeper_tick` (every 60 s).
- **Reducers:** `set_agent_config`, `agents_set_api_key`, `agents_clear_api_key`, `set_agent_override`,
  `clear_agent_override`, `add_agent_admin_identity`, `remove_agent_admin_identity`, `agents_clear_thread_lock`, and
  the scheduled `agents_thread_lock_sweep`.
- **Renamed** to carry the package, since the TS names are generic (TS mounts them under a namespace): `api_key`,
  `thread`, `message`, `thread_lock`, `thread_lock_sweeper_tick` and `message_embedding` get an `agents_` prefix, and
  so do `set_api_key`, `clear_api_key`, `clear_thread_lock` and `thread_lock_sweep`. `api_key` would also collide with
  the [api-keys](../api-keys) port.

**Same behaviour as TS:**
- **The model loop:** turns, tool calls run inside the host's transaction, results fed back, `finish_reason` gating,
  max turns, cancellation between turns, regeneration, and the thread lock that makes a busy thread reject new runs.
- **Providers:** OpenRouter, OpenAI and Anthropic request bodies are **byte-identical** to TS (system prompts, tools,
  `tool_choice`, `max_tokens`, `response_format`, images, tool results). Response parsing matches too: tool calls,
  usage coercion, finish reasons and errors. Immediate retries on 429, 5xx and transport errors.
- **Tools:** the JSON Schema each tool sends, and argument validation with the same paths and messages (`$.point.y is
  required`, `$.u8 must be within the U8 range`…). Also the 64-tool, 1,024-char description, 64 KB input/result and
  1,000-item caps.
- **Memory:** the history window (dropping orphaned tool results), running summaries by a summarizer agent, embedding
  recall (cosine top-k outside the window), and the `<conversation_context>` message that carries both.
- **Everything else:** title generation and cleanup, operator overrides, per-thread model selection, per-owner thread
  caps, usage hooks, admin reducers, the stale-lock sweep, and every `agents.*` error code.
- **JSON** that TS re-serializes (Anthropic tool inputs, response formats) keeps JS's property order (integer-like
  keys first) and number formatting (`1.0` → `1`, `1e21` → `1e+21`).

```rust
fn client() -> &'static agents::AgentsClient {
    static CLIENT: std::sync::OnceLock<agents::AgentsClient> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let get_time = agents::agent_tool("Returns the server time.", agents::ArgType::Unit, |tx, _| {
            Ok(tx.timestamp.to_micros_since_unix_epoch().to_string())
        }).unwrap();
        let chat = agents::define_agent(agents::AgentSpec {
            default_model: "anthropic/claude-haiku-4.5".into(),
            tools: vec![("get_time", get_time)],
            ..Default::default()
        }).unwrap();
        agents::AgentsClient::new(agents::AgentsConfig { agents: vec![("chat", chat)], ..Default::default() }).unwrap()
    })
}

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) { agents::install(ctx); }   // the publisher becomes the first admin

#[spacetimedb::procedure]
pub fn send_message(ctx: &mut ProcedureContext, thread_id: u64, content: String) -> Result<(), String> {
    let owner = ctx.sender().to_hex().to_string();
    client().send_message(ctx, &owner, thread_id, &content, None)
}
```

```csharp
static AgentsClient? client;
static AgentsClient Client => client ??= new(new AgentsConfig
{
    Agents = [("chat", AgentDefinition.Define(new AgentSpec
    {
        DefaultModel = "anthropic/claude-haiku-4.5",
        Tools = [("get_time", AgentTool.Create("Returns the server time.", AgentArg.Unit, (tx, _) => tx.Timestamp.ToString()))],
    }))],
});

[SpacetimeDB.Reducer(ReducerKind.Init)]
public static void Init(ReducerContext ctx) => AgentsInstall(ctx);

[SpacetimeDB.Procedure]
public static Result<AgentsUnit, string> SendMessage(ProcedureContext ctx, ulong threadId, string content) =>
    AgentsTry(() => Client.SendMessage(ctx, ctx.Sender.ToString(), threadId, content));
```

Then, as an admin: `agents_set_api_key("openrouter", "<key>")`.

| TypeScript | Rust | C# |
|---|---|---|
| `agentTool(description, t.object({...}), run)` | `agent_tool(description, ArgType::object(&[...]), fn)` | `AgentTool.Create(description, AgentArg.Object(...), run)` |
| `defineAgent({...})` | `define_agent(AgentSpec {...})` | `AgentDefinition.Define(new AgentSpec {...})` |
| `client({ agents, submodule, beforeRun, onUsage, attachments, maxThreadsPerOwner })` | `AgentsClient::new(AgentsConfig {...})` | `new AgentsClient(new AgentsConfig {...})` |
| `startThread` / `updateThread` / `deleteThread` / `requestCancel` | `start_thread` / `update_thread` / `delete_thread` / `request_cancel` | `StartThread` / `UpdateThread` / `DeleteThread` / `RequestCancel` |
| `sendMessage(ctx, { owner, threadId, content, onInsert })` | `send_message(ctx, owner, thread_id, content, Some(&on_insert))` | `SendMessage(ctx, owner, threadId, content, onInsert)` |
| `regenerateResponse` / `generateThreadTitle` / `status` / `isAdmin` | `regenerate_response` / `generate_thread_title` / `status` / `is_admin` | `RegenerateResponse` / `GenerateThreadTitle` / `Status` / `IsAdmin` |
| `install(ctx)` | `install(ctx)` | `AgentsInstall(ctx)` |
| `callChat`, providers, `isRetryableError` | `call_chat`, `Provider`, `is_retryable_error` | `AgentsChat.Call`, `AgentProvider`, `ChatError.IsRetryable` |
| embedding providers, `cosineSimilarity`, `topKByScore` | `embed`, `cosine_similarity`, `top_k_by_score` | `AgentsChat.Embed`, `CosineSimilarity`, `TopKByScore` |
| `errors` | `agents::errors` | `AgentsErrors` |

**Differences:**
- **Tool arguments:** declared with `ArgType` / `AgentArg` (the `t.*` builders a tool can use: scalars, objects,
  options, arrays, tagged enums, unit). They arrive as validated JSON values. 64-bit integers stay numbers (TS makes
  them `BigInt`s). Rust tools return `Result<String, String>`; C# tools throw, like TS.
- **Tool context:** Rust tools get the host's `&ReducerContext`. C# tools get an `AgentsTx` (database, timestamp,
  sender). TS passes the host's transaction.
- **Hooks:** plain functions over the host transaction. `before_run` returns `Err` to reject a run; C#'s `BeforeRun`
  throws, as in TS.
- **Validation timing:** definitions are validated when built (`define_agent`, `AgentsClient::new`), so a bad tool name
  fails in `define_agent`. TS fails when `client()` builds its registry; the messages are the same.
- **Provider URLs:** they can be overridden per name (`AgentsConfig::urls` / `Urls`), so a test double can stand in for
  a provider. The defaults are the TS URLs.
- **The scheduled sweep** rejects callers other than the scheduler.
- **Rust panics:** a tool that panics aborts the procedure before the thread lock is released. The stale-lock sweep or
  `agents_clear_thread_lock` frees it. TS releases the lock in a `finally`; C# does too.
- **JSON.parse messages** come from `serde_json` / `System.Text.Json`, not V8. That affects the text after
  `invalid JSON in tool input:` and the parse error in the module log; the stored error code is the same.
- **Lone-surrogate escapes** (`"\ud800"`) in provider JSON fail to parse, where JS keeps them. Rust also keeps integers
  beyond 2^53 exact in re-serialized JSON, where JS rounds them.
- **Streaming:** TS doesn't stream either. Module HTTP is request/response, so every port waits for the whole
  completion.

**Demo:** the upstream example module, with these changes:
- **Extra agents:** `kitchen` (every tool argument kind) and `claude` (Anthropic, with a tool that writes a row).
- **Tests:** a per-owner cap of 5 threads, to make the cap testable.
- **Renames:** the example's `File` view row is `AttachmentFile`, since `File` is the files port's table row.

**Tests:** [`tests/agents.test.mjs`](../tests/agents.test.mjs) has 25 suites:
- **Vectors from the real TS code** ([`tests/vectors/agents.json`](../tests/vectors/agents.json), from
  [`tools/gen-agents-vectors.mjs`](../tools/gen-agents-vectors.mjs)): 61 tool invocations, schemas, definition
  errors, 21 provider request bodies, 31 response parses, retries, 30 embedding calls, history windows, summaries and
  context.
- **The example end to end** against mock OpenRouter, OpenAI and Anthropic endpoints on `127.0.0.1:4320`. No real
  provider or key is used. Module HTTP to loopback needs the [dev server](../scripts/dev-server.sh).
