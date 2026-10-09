// Generate tests/vectors/agents.json from the real @spacetimedb/agents code (src/ at the pinned
// commit 37ee2ad): tool JSON Schemas, tool input validation and invoke results, definition
// errors, provider request bodies and response parsing, retry behaviour, embeddings parsing,
// cosine/top-k, history building and summarizer/context text. The Rust and C# ports must
// reproduce every value.
//
//   UPSTREAM=<SpacetimeDB checkout at 37ee2ad>/spacetime-agents-ts OUT=tests/vectors/agents.json \
//     npx tsx tools/gen-agents-vectors.mjs
//
// Run it from a directory where `spacetimedb` (2.11, for the `t` builders) resolves, with
// "type": "module". Nothing here imports spacetimedb/server.
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { t } from 'spacetimedb';

const upstream = process.env.UPSTREAM;
if (!upstream) throw new Error('set UPSTREAM to the spacetime-agents-ts package directory');
const src = (f) => import(pathToFileURL(join(upstream, 'src', f)).href);
const { agentTool, makeAgentDispatch, defineAgent, makeAgentRegistry, typeBuilderToJsonSchema } = await src('agent.ts');
const { BUILT_IN_PROVIDERS } = await src('providers.ts');
const { callChat } = await src('openrouter.ts');
const { BUILT_IN_EMBEDDING_PROVIDERS, cosineSimilarity, topKByScore } = await src('embeddings.ts');
const { buildLlmMessages } = await src('submodule/loop.ts');
const { pickSummarizationCandidates, buildSummarizerUserContent, buildContextMessage } = await src('submodule/summarize.ts');

const json = (v) => JSON.stringify(v, (_k, x) => (typeof x === 'bigint' ? Number(x) : x));
const errorOf = (f) => {
  try {
    f();
  } catch (e) {
    return e.message;
  }
  throw new Error('expected an error');
};

// --- Tools: the demo modules' `kitchen` agent defines the same tools. ---------------------
const echoArgs = (_ctx, args) => json(args);
const tools = {
  echo: agentTool('Echo the validated arguments.', t.object('EchoArgs', { msg: t.string(), n: t.option(t.u64()) }), echoArgs),
  kinds: agentTool(
    '  Every scalar kind.  ',
    t.object('KindsArgs', {
      b: t.bool(), s: t.string(), f: t.f64(), g: t.f32(),
      i8: t.i8(), u8: t.u8(), i16: t.i16(), u16: t.u16(), i32: t.i32(), u32: t.u32(), i64: t.i64(), u64: t.u64(),
    }),
    echoArgs
  ),
  nested: agentTool(
    'Nested objects, arrays and options.',
    t.object('NestedArgs', {
      point: t.object('Point', { x: t.i32(), y: t.i32() }),
      tags: t.array(t.string()),
      maybe: t.option(t.array(t.u8())),
    }),
    echoArgs
  ),
  shape: agentTool(
    'A tagged union.',
    t.object('ShapeArgs', {
      shape: t.enum('Shape', { circle: t.f64(), square: t.object('Square', { side: t.f64() }), none: t.unit() }),
      note: t.option(t.string()),
    }),
    echoArgs
  ),
  nothing: agentTool('Takes no arguments.', t.unit(), () => 'nothing to see'),
  fail: agentTool('Always throws.', t.object('FailArgs', { reason: t.string() }), (_c, a) => {
    throw new Error(`boom: ${a.reason}`);
  }),
  big: agentTool('Returns n characters.', t.object('BigArgs', { n: t.u32() }), (_c, a) => 'x'.repeat(a.n)),
};
const dispatch = makeAgentDispatch(tools);
const toolDefs = dispatch.llmToolDefs;

const invokeInputs = [
  ['echo', '{"msg":"hi"}'],
  ['echo', '{"msg":"hi","n":5}'],
  ['echo', ''],
  ['echo', '{}'],
  ['echo', '{"msg":1}'],
  ['echo', '{"msg":"hi","extra":true}'],
  ['echo', '{"msg":"hi","n":-1}'],
  ['echo', '{"msg":"hi","n":1.5}'],
  ['echo', '{"msg":"hi","n":9007199254740992}'],
  ['echo', '{"msg":"hi","n":null}'],
  ['echo', '[]'],
  ['echo', '"str"'],
  ['echo', 'null'],
  ['kinds', '{"b":true,"s":"x","f":1.5,"g":-0.25,"i8":-128,"u8":255,"i16":-32768,"u16":65535,"i32":-2147483648,"u32":4294967295,"i64":-9007199254740991,"u64":9007199254740991}'],
  ['kinds', '{"b":false,"s":"","f":1.0,"g":1e-7,"i8":-0,"u8":0,"i16":0,"u16":0,"i32":0,"u32":0,"i64":0,"u64":0}'],
  ['kinds', '{"b":false,"s":"","f":1e21,"g":123e-20,"i8":0,"u8":0,"i16":0,"u16":0,"i32":0,"u32":0,"i64":0,"u64":0}'],
  ['kinds', '{"b":false,"s":"","f":-2.50e2,"g":5e-324,"i8":0,"u8":0,"i16":0,"u16":0,"i32":0,"u32":0,"i64":1e3,"u64":1.0}'],
  ['kinds', '{"b":false,"s":"","f":0.000001,"g":1e300,"i8":0,"u8":0,"i16":0,"u16":0,"i32":0,"u32":0,"i64":0,"u64":0}'],
  ['kinds', '{"b":1,"s":"x","f":1.5,"g":1,"i8":0,"u8":0,"i16":0,"u16":0,"i32":0,"u32":0,"i64":0,"u64":0}'],
  ['kinds', '{"b":true,"s":"x","f":"1","g":1,"i8":0,"u8":0,"i16":0,"u16":0,"i32":0,"u32":0,"i64":0,"u64":0}'],
  ['kinds', '{"b":true,"s":"x","f":1.5,"g":1,"i8":128,"u8":0,"i16":0,"u16":0,"i32":0,"u32":0,"i64":0,"u64":0}'],
  ['kinds', '{"b":true,"s":"x","f":1.5,"g":1,"i8":0,"u8":-1,"i16":0,"u16":0,"i32":0,"u32":0,"i64":0,"u64":0}'],
  ['kinds', '{"b":true,"s":"x","f":1.5,"g":1,"i8":0,"u8":0,"i16":40000,"u16":0,"i32":0,"u32":0,"i64":0,"u64":0}'],
  ['kinds', '{"b":true,"s":"x","f":1.5,"g":1,"i8":0,"u8":0,"i16":0,"u16":65536,"i32":0,"u32":0,"i64":0,"u64":0}'],
  ['kinds', '{"b":true,"s":"x","f":1.5,"g":1,"i8":0,"u8":0,"i16":0,"u16":0,"i32":2147483648,"u32":0,"i64":0,"u64":0}'],
  ['kinds', '{"b":true,"s":"x","f":1.5,"g":1,"i8":0,"u8":0,"i16":0,"u16":0,"i32":0,"u32":4294967296,"i64":0,"u64":0}'],
  ['kinds', '{"b":true,"s":"x","f":1.5,"g":1,"i8":0,"u8":0,"i16":0,"u16":0,"i32":0,"u32":0,"i64":9007199254740992,"u64":0}'],
  ['kinds', '{"b":true,"s":"x","f":1.5,"g":1,"i8":0,"u8":0,"i16":0,"u16":0,"i32":0,"u32":0,"i64":0,"u64":-5}'],
  ['kinds', '{"b":true,"s":"x","f":1.5,"g":1,"i8":0.5,"u8":0,"i16":0,"u16":0,"i32":0,"u32":0,"i64":0,"u64":0}'],
  ['nested', '{"point":{"x":1,"y":-2},"tags":["a","b"]}'],
  ['nested', '{"point":{"y":-2,"x":1,"x":3},"tags":["a"],"tags":["b"]}'],
  ['nested', '{"tags":[],"point":{"y":2,"x":1},"maybe":[1,2,255]}'],
  ['nested', '{"point":{"x":1},"tags":[]}'],
  ['nested', '{"point":{"x":1,"y":2,"z":3},"tags":[]}'],
  ['nested', '{"point":[1,2],"tags":[]}'],
  ['nested', '{"point":{"x":1,"y":2},"tags":["a",2]}'],
  ['nested', '{"point":{"x":1,"y":2},"tags":"a"}'],
  ['nested', '{"point":{"x":1,"y":2},"tags":[],"maybe":[1,256]}'],
  ['nested', `{"point":{"x":1,"y":2},"tags":[${Array(1001).fill('"t"').join(',')}]}`],
  ['nested', `{"point":{"x":1,"y":2},"tags":[${Array(1000).fill('"t"').join(',')}]}`],
  ['shape', '{"shape":{"tag":"circle","value":2.5}}'],
  ['shape', '{"shape":{"tag":"square","value":{"side":3}},"note":"n"}'],
  ['shape', '{"shape":{"tag":"none"}}'],
  ['shape', '{"shape":{"tag":"none","value":1}}'],
  ['shape', '{"shape":{"tag":"circle"}}'],
  ['shape', '{"shape":{"tag":"circle","value":1,"x":2}}'],
  ['shape', '{"shape":{"tag":"hexagon","value":1}}'],
  ['shape', '{"shape":{"tag":3}}'],
  ['shape', '{"shape":"circle"}'],
  ['shape', '{"shape":{"tag":"square","value":{"side":"3"}}}'],
  ['shape', '{"note":"n"}'],
  ['nothing', ''],
  ['nothing', '{}'],
  ['nothing', '{"a":1}'],
  ['fail', '{"reason":"no"}'],
  ['fail', '{"reason":5}'],
  ['big', '{"n":65536}'],
  ['big', '{"n":65537}'],
  ['missing', '{}'],
  ['constructor', '{}'],
  ['echo', `{"msg":"${'a'.repeat(65530)}"}`],
];
const invokes = invokeInputs.map(([tool, input]) => {
  const r = dispatch.invoke({}, tool, input);
  const row = { tool, input, isError: r.isError };
  if (r.result.length > 200) row.resultLength = r.result.length;
  else row.result = r.result;
  return row;
});
// Engine-specific JSON.parse messages: only the prefix is comparable.
const syntaxInvokes = ['{', '{"msg":', 'nope', '{"msg":"a",}'].map((input) => {
  const r = dispatch.invoke({}, 'echo', input);
  return { tool: 'echo', input, isError: r.isError, resultPrefix: 'invalid JSON in tool input: ' };
});

// --- Definition errors ------------------------------------------------------------------
const okTool = tools.nothing;
const definitionErrors = {
  u128: errorOf(() => typeBuilderToJsonSchema(t.object('Wide', { x: t.u128() }))),
  i128: errorOf(() => typeBuilderToJsonSchema(t.object('Wide', { x: t.option(t.i128()) }))),
  top_string: errorOf(() => typeBuilderToJsonSchema(t.string())),
  top_array: errorOf(() => typeBuilderToJsonSchema(t.array(t.string()))),
  bad_tool_name: errorOf(() => makeAgentDispatch({ 'has spaces': okTool })),
  long_tool_name: errorOf(() => makeAgentDispatch({ ['a'.repeat(65)]: okTool })),
  too_many_tools: errorOf(() => makeAgentDispatch(Object.fromEntries(Array.from({ length: 65 }, (_, i) => [`t${i}`, okTool])))),
  description_empty: errorOf(() => agentTool('   ', t.unit(), () => '')),
  description_long: errorOf(() => agentTool('d'.repeat(1025), t.unit(), () => '')),
  model_empty: errorOf(() => defineAgent({ defaultModel: '  ', tools: {} })),
  model_long: errorOf(() => defineAgent({ defaultModel: 'm'.repeat(257), tools: {} })),
  provider_long: errorOf(() => defineAgent({ defaultProvider: 'p'.repeat(65), defaultModel: 'm', tools: {} })),
  prompt_long: errorOf(() => defineAgent({ defaultModel: 'm', defaultSystemPrompt: 's'.repeat(32769), tools: {} })),
  turns_zero: errorOf(() => defineAgent({ defaultModel: 'm', defaultMaxTurns: 0, tools: {} })),
  turns_big: errorOf(() => defineAgent({ defaultModel: 'm', defaultMaxTurns: 101, tools: {} })),
  history_big: errorOf(() => defineAgent({ defaultModel: 'm', defaultMaxHistoryMessages: 1001, tools: {} })),
  tokens_zero: errorOf(() => defineAgent({ defaultModel: 'm', defaultMaxTokens: 0, tools: {} })),
  retries_big: errorOf(() => defineAgent({ defaultModel: 'm', defaultRetries: 11, tools: {} })),
  rag_big: errorOf(() => defineAgent({ defaultModel: 'm', ragTopK: 101, tools: {} })),
  models_empty: errorOf(() => defineAgent({ defaultModel: 'm', models: [' '], tools: {} })),
  summarizer_empty: errorOf(() => defineAgent({ defaultModel: 'm', summarizerAgentName: '', tools: {} })),
  embeddings_model_long: errorOf(() => defineAgent({ defaultModel: 'm', embeddingsModel: 'e'.repeat(257), tools: {} })),
  bad_agent_name: errorOf(() => makeAgentRegistry({ 'a b': defineAgent({ defaultModel: 'm', tools: {} }) })),
  // From client(): not importable without spacetimedb/server; the text is the TS source's.
  unknown_summarizer: "agent 'chat' names unknown summarizer 'nobody'",
  max_threads_zero: 'maxThreadsPerOwner must be a positive integer',
};
const defaults = (() => {
  const a = defineAgent({ defaultModel: '  m/x  ', models: [' a ', 'b'], summarizerAgentName: ' s ', tools: {} });
  const { tools: _t, ...rest } = a;
  return json(rest);
})();

// --- Providers --------------------------------------------------------------------------
const toolCall = (id, name, args) => ({ id, type: 'function', function: { name, arguments: args } });
const requests = [
  { apiKey: 'sk-test', model: 'some/model', messages: [{ role: 'user', content: 'hi' }] },
  {
    apiKey: 'sk-test', model: 'some/model', system: 'Be brief.', maxTokens: 256,
    messages: [{ role: 'user', content: 'hi' }], tools: toolDefs.slice(0, 2),
  },
  { apiKey: 'k', model: 'm', system: '', messages: [{ role: 'user', content: '' }], responseFormat: { type: 'json_object' } },
  {
    apiKey: 'k', model: 'm', system: 'sys',
    messages: [
      { role: 'system', content: 'inline system' },
      { role: 'user', content: 'what time is it?' },
      { role: 'assistant', content: '', tool_calls: [toolCall('call_1', 'nothing', '{}'), toolCall('call_2', 'echo', 'not json')] },
      { role: 'tool', tool_call_id: 'call_1', content: 'nothing to see' },
      { role: 'tool', tool_call_id: 'call_2', content: 'x' },
      { role: 'assistant', content: 'thinking', tool_calls: [toolCall('call_3', 'echo', '{"msg":"a","n":2}')] },
      { role: 'assistant', content: 'done' },
    ],
    tools: toolDefs,
  },
  {
    apiKey: 'k', model: 'vision', maxTokens: 1,
    messages: [
      { role: 'user', content: [{ type: 'text', text: 'look' }, { type: 'image', mimeType: 'image/png', data: 'iVBORw0KGgo=' }] },
      { role: 'user', content: [{ type: 'image', mimeType: 'image/jpeg', data: '/9j/' }] },
      { role: 'assistant', content: [{ type: 'text', text: '' }, { type: 'text', text: 'ok' }], tool_calls: [toolCall('c', 'nothing', '')] },
    ],
  },
  {
    apiKey: 'k', model: 'm', responseFormat: { type: 'json_schema', scale: 1.0, tiny: 1e-7, huge: 1e21 },
    messages: [{ role: 'assistant', content: 'x', tool_calls: [toolCall('n', 'kinds', '{"a":1.0,"b":1e21,"c":1e-7,"d":2.50,"e":-0,"f":1e300,"g":5e-324,"h":123e-20,"i":-0.0,"j":0.1,"k":100,"l":1e20}'), toolCall('o', 'kinds', '{"z":1,"5":2,"01":3,"3":{"y":0,"1":1},"z":9}')] }],
  },
  { apiKey: 'k', model: 'm', messages: [{ role: 'user', content: 'unicode é \u{1f600} "quoted" \\ \n\t\u0001' }], tools: [] },
];
const builds = [];
for (const provider of ['openrouter', 'openai', 'anthropic']) {
  for (const [i, req] of requests.entries()) {
    const b = BUILT_IN_PROVIDERS[provider].buildRequest(req);
    builds.push({ provider, request: i, url: b.url, headers: Object.entries(b.headers), body: b.body });
  }
}

const openAiResponses = [
  { model: 'gpt-x', choices: [{ message: { role: 'assistant', content: 'hello back' }, finish_reason: 'stop' }], usage: { prompt_tokens: 12, completion_tokens: 5, total_tokens: 17 } },
  { choices: [{ message: { content: null, tool_calls: [{ id: 'call_1', type: 'function', function: { name: 'echo', arguments: '{"msg":"x"}' } }, { function: { name: 7 } }, 'junk'] }, finish_reason: 'tool_calls' }] },
  { choices: [{ message: { content: ['not', 'a string'] } }], usage: { prompt_tokens: '12', completion_tokens: -3, total_tokens: 'abc' }, model: 42 },
  { choices: [{}], usage: { prompt_tokens: 1e300, completion_tokens: 2.9, total_tokens: null } },
  { choices: [{ message: { content: 'x' }, finish_reason: 'length' }], model: null },
  { choices: [] },
  { choices: ['x'] },
  [],
  null,
  { choices: [{ message: { content: 'partial' }, finish_reason: null }], usage: { prompt_tokens: true } },
];
const anthropicResponses = [
  { model: 'claude-3-5-sonnet', stop_reason: 'end_turn', content: [{ type: 'text', text: 'hello back' }], usage: { input_tokens: 12, output_tokens: 5 } },
  { stop_reason: 'tool_use', content: [{ type: 'text', text: 'using tool' }, { type: 'tool_use', id: 'tu_1', name: 'echo', input: { msg: 'x', n: 2 } }, { type: 'tool_use', id: 'tu_2', name: 'nothing' }] },
  { stop_reason: 'max_tokens', content: [{ type: 'text', text: 'a' }, { type: 'text' }, { type: 'text', text: 'b' }, 'junk', null] },
  { stop_reason: 'stop_sequence', content: [] },
  { stop_reason: 'refusal', content: [{ type: 'thinking', thinking: 'hm' }], usage: { input_tokens: '3', output_tokens: 4.5 } },
  { content: [{ type: 'tool_use', id: 1, name: null, input: { nested: { a: [1, 'b', null, true] } } }] },
  '{"content":[{"type":"tool_use","id":"f","name":"n","input":{"a":1.0,"b":1e21,"c":-1e-7,"d":12.50,"e":1E5}}]}',
  '{"content":[{"type":"tool_use","id":"k","name":"n","input":{"b":1,"10":2,"2":3,"01":4,"4294967295":5,"4294967294":6,"a":{"1":0,"x":1,"0":2},"b":7}}]}',
  { content: 'nope' },
  {},
  [],
];
const parse = (provider, value) => {
  const text = typeof value === 'string' ? value : JSON.stringify(value);
  try {
    const { raw: _raw, ...r } = BUILT_IN_PROVIDERS[provider].parseResponse(text, 'requested-model');
    return { provider, text, ok: r };
  } catch (e) {
    return { provider, text, error: e.message };
  }
};
const parses = [
  ...openAiResponses.flatMap((r) => ['openrouter', 'openai'].map((p) => parse(p, r))),
  ...anthropicResponses.map((r) => parse('anthropic', r)),
];
// Syntax errors: messages are engine-specific, so only "is an error" is compared.
const parseSyntax = ['', '{', 'not json'].flatMap((text) => ['openai', 'anthropic'].map((provider) => ({ provider, text })));

// --- callChat retries over a scripted transport ----------------------------------------
const okBody = JSON.stringify({ model: 'm', choices: [{ message: { content: 'ok' }, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } });
const scripts = [
  { retries: 2, responses: [[503, 'busy'], [503, 'busy'], [200, okBody]] },
  { retries: 2, responses: [[429, 'slow'], [429, 'slow'], [429, 'slow'], [200, okBody]] },
  { retries: 0, responses: [[503, 'busy'], [200, okBody]] },
  { retries: 3, responses: [[401, 'denied'], [200, okBody]] },
  { retries: 1, responses: [['throw', 'connection refused'], [200, okBody]] },
  { retries: 1, responses: [['throw', 'connection refused'], ['throw', 'timeout']] },
  { retries: 2, responses: [[200, '{"choices":[]}'], [200, okBody]] },
  { retries: 1, responses: [[599, 'x'], [600, 'y'], [200, okBody]] },
  { retries: 0, responses: [[204, okBody]] },
  { retries: 0, responses: [[199, okBody]] },
];
const calls = scripts.map(({ retries, responses }) => {
  let n = 0;
  const http = {
    fetch() {
      const [status, body] = responses[n++];
      if (status === 'throw') throw new Error(body);
      return { status, text: () => body };
    },
  };
  const r = callChat(http, BUILT_IN_PROVIDERS.openai, { apiKey: 'k', model: 'm', messages: [{ role: 'user', content: 'hi' }], retries });
  const result = r.ok ? { ok: true, text: r.response.text } : { ok: false, error: r.error };
  return { retries, responses, attempts: n, result };
});

// --- Embeddings --------------------------------------------------------------------------
const embedCases = [
  { texts: ['hello'], status: 200, body: JSON.stringify({ model: 'text-embedding-3-small', data: [{ embedding: [0.1, -0.2, 0.3] }], usage: { prompt_tokens: 8, total_tokens: 8 } }) },
  { texts: ['a', 'b'], status: 200, body: JSON.stringify({ data: [{ embedding: [1, 2] }, { embedding: [3, 4] }] }) },
  { texts: ['a'], status: 200, body: JSON.stringify({ data: [{ embedding: [] }] }) },
  { texts: ['a'], status: 200, body: JSON.stringify({ data: [{ embedding: [1, 'x'] }] }) },
  { texts: ['a'], status: 200, body: JSON.stringify({ data: [{}] }) },
  { texts: ['a'], status: 200, body: JSON.stringify({ data: 'x' }) },
  { texts: ['a'], status: 200, body: JSON.stringify([1]) },
  { texts: ['a'], status: 200, body: JSON.stringify({ data: [{ embedding: Array(16385).fill(0) }] }) },
  { texts: ['a'], status: 401, body: 'bad key' },
  { texts: ['a'], status: 500, body: 'e'.repeat(70000) },
  { texts: [], status: 200, body: '' },
  { texts: Array(101).fill('a'), status: 200, body: '' },
  { texts: ['a'.repeat(32769)], status: 200, body: '' },
  { texts: Array(9).fill('a'.repeat(30000)), status: 200, body: '' },
  { texts: ['a'], status: 200, body: JSON.stringify({ data: [{ embedding: [1] }], usage: { prompt_tokens: '4', total_tokens: -1 }, model: 9 }) },
];
const embeds = [];
for (const provider of ['openai', 'openrouter']) {
  for (const c of embedCases) {
    let sent = null;
    const http = {
      fetch(url, init) {
        sent = { url, headers: Object.entries(init.headers), body: init.body };
        return { status: c.status, text: () => c.body };
      },
    };
    const r = BUILT_IN_EMBEDDING_PROVIDERS[provider].embed(http, 'ek', 'emb-model', c.texts);
    const row = { provider, texts: c.texts, status: c.status, body: c.body, sent, result: r };
    if (r.ok === false && 'body' in r.error && r.error.body.length > 200) {
      row.result = { ok: false, error: { ...r.error, body: undefined, bodyLength: r.error.body.length } };
    }
    embeds.push(row);
  }
}
const cosine = [
  [[1, 0], [1, 0]], [[1, 0], [0, 1]], [[1, 2, 3], [-1, -2, -3]], [[1, 2], [1, 2, 3]], [[], []], [[0, 0], [1, 1]], [[0.5, 0.25, -1], [2, 0.1, 0.3]],
].map(([a, b]) => ({ a, b, score: cosineSimilarity(a, b) }));
const topK = [
  { scores: [0.1, 0.9, 0.5, 0.9, -1], k: 2 },
  { scores: [3, 1, 2], k: 0 },
  { scores: [3, 1, 2], k: 10 },
].map((c) => ({ ...c, out: topKByScore(c.scores.map((s, i) => i), (i) => c.scores[i], c.k).map((r) => r.item) }));

// --- History, summaries and context ------------------------------------------------------
let nextId = 1n;
const msg = (role, content, extra = {}) => ({
  id: nextId++, threadId: 1n, role, content, toolCallsJson: undefined, toolCallId: undefined, isError: false,
  promptTokens: undefined, completionTokens: undefined, attachments: [], ...extra,
});
const tc = (...calls) => JSON.stringify(calls.map(([id, name, args]) => toolCall(id, name, args)));
const thread = [
  msg('user', 'hi'),
  msg('assistant', 'hello'),
  msg('user', 'what time?', { attachments: [{ mimeType: 'image/png', data: 'AAAA' }] }),
  msg('assistant', '', { toolCallsJson: tc(['c1', 'get_time', '{}'], ['c2', 'echo', '{"msg":"x"}']) }),
  msg('tool', '12:00', { toolCallId: 'c1' }),
  msg('tool', 'x', { toolCallId: 'c2', isError: true }),
  msg('tool', 'orphan', { toolCallId: 'zzz' }),
  msg('assistant', 'It is noon.', { promptTokens: 5, completionTokens: 2 }),
  msg('assistant', 'bad json', { toolCallsJson: '{nope' }),
  msg('assistant', 'empty calls', { toolCallsJson: '[]' }),
  msg('system', 'ignored role'),
  msg('user', '', { attachments: [{ mimeType: 'image/jpeg', data: 'BBBB' }] }),
  msg('assistant', 'calls again', { toolCallsJson: tc(['c3', 'echo', '{"msg":"y"}']) }),
  msg('tool', 'y', { toolCallId: 'c3' }),
  msg('tool', 'no id'),
];
const view = (m) => ({ id: m.id, role: m.role, content: m.content, toolCallsJson: m.toolCallsJson ?? null, toolCallId: m.toolCallId ?? null, attachments: m.attachments });
const historyTx = (all) => ({ listMessages: () => all });
const histories = [0, 1, 3, 4, 5, 8, 10, 15, 50].map((max) => ({ max, out: buildLlmMessages(historyTx(thread), 1n, max) }));
const summaries = [
  { max: 4, through: null, existing: null },
  { max: 4, through: 5, existing: 'Earlier: greetings.' },
  { max: 4, through: 11, existing: 'All of it.' },
  { max: 15, through: null, existing: null },
  { max: 20, through: null, existing: null },
  { max: 0, through: null, existing: '' },
].map((c) => {
  const picked = pickSummarizationCandidates(thread, c.max, c.through == null ? null : BigInt(c.through));
  return {
    ...c,
    lastNewId: picked ? Number(picked.lastNewId) : null,
    content: picked ? buildSummarizerUserContent(c.existing, picked.newDropped) : null,
  };
});
const oddCalls = [
  msg('assistant', 'odd', { toolCallsJson: '[1,{"function":{}},{"function":{"name":5,"arguments":{"a":1}}}]' }),
  msg('assistant', '', { toolCallsJson: 'null' }),
  msg('tool', 'r'),
];
const oddSummary = buildSummarizerUserContent(null, oddCalls);
const contexts = [
  [undefined, []],
  ['', []],
  ['The user likes tea.', []],
  [undefined, ['[user] a', '[assistant] b']],
  ['S', ['[user] only']],
].map(([summary, snippets]) => ({ summary: summary ?? null, snippets, out: buildContextMessage(summary, snippets) ?? null }));

const out = {
  source: 'SpacetimeDB PR #5905 @ 37ee2ad627568f95df8229647cff5a482d629c25, spacetime-agents-ts',
  tools: { defs: toolDefs, invokes, syntaxInvokes },
  definitionErrors,
  defaults: JSON.parse(defaults),
  providers: { requests, builds, parses, parseSyntax },
  calls,
  embeddings: { cases: embeds, cosine, topK },
  history: { messages: thread.map(view), histories, summaries, oddCalls: oddCalls.map(view), oddSummary, contexts },
};
writeFileSync(process.env.OUT ?? new URL('../tests/vectors/agents.json', import.meta.url), json(out) + '\n');
console.log('wrote tests/vectors/agents.json');
