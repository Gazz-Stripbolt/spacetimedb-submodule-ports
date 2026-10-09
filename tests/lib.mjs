// Shared helpers for the port test suites. Everything goes through SpacetimeDB's HTTP API,
// so one suite drives the Rust and C# demo modules identically.
//
//   HOST (default http://127.0.0.1:3000), DB (the published demo database)
//   OWNER_TOKEN (default: read from ~/.config/spacetime/cli.toml, the identity that published)
import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

export const HOST = process.env.HOST ?? 'http://127.0.0.1:3000';
export const DB = process.env.DB;
if (!DB) throw new Error('set DB to the published demo database');

function ownerToken() {
  if (process.env.OWNER_TOKEN) return process.env.OWNER_TOKEN;
  const dir = process.env.SPACETIME_CONFIG_DIR ?? join(homedir(), '.config', 'spacetime');
  const toml = readFileSync(join(dir, 'cli.toml'), 'utf8');
  const m = toml.match(/spacetimedb_token\s*=\s*"([^"]+)"/);
  if (!m) throw new Error('no spacetimedb_token in cli.toml; set OWNER_TOKEN');
  return m[1];
}

export const owner = { token: ownerToken(), identity: null };

/** A fresh identity (not an admin of anything). */
export async function newIdentity() {
  const r = await fetch(`${HOST}/v1/identity`, { method: 'POST' });
  if (!r.ok) throw new Error(`POST /v1/identity: ${r.status}`);
  return r.json(); // { identity, token }
}

/**
 * Call a reducer or procedure. Resolves to the procedure's JSON result (or null for reducers);
 * rejects with an Error whose message is the module's error text.
 */
export async function call(name, args = [], who = owner) {
  const r = await fetch(`${HOST}/v1/database/${DB}/call/${name}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...(who?.token ? { authorization: `Bearer ${who.token}` } : {}) },
    body: JSON.stringify(args),
  });
  const text = await r.text();
  if (!r.ok) throw new Error(text || `HTTP ${r.status}`);
  return text.length ? JSON.parse(text) : null;
}

/** The error message a call fails with, or null if it succeeds. */
export async function callErr(name, args = [], who = owner) {
  try {
    await call(name, args, who);
    return null;
  } catch (e) {
    return e.message;
  }
}

/** For procedures returning a Result: the `err` value, or null if `ok`. HTTP failures count too. */
export async function procErr(name, args = [], who = owner) {
  try {
    const r = await call(name, args, who);
    return r[0] === 1 ? String(r[1]) : null;
  } catch (e) {
    return e.message;
  }
}

/** Run SQL as the owner (sees private tables). Returns rows as objects. */
export async function sql(query, who = owner) {
  const r = await fetch(`${HOST}/v1/database/${DB}/sql`, {
    method: 'POST',
    headers: { authorization: `Bearer ${who.token}` },
    body: query,
  });
  const text = await r.text();
  if (!r.ok) throw new Error(text || `HTTP ${r.status}`);
  const [res] = JSON.parse(text);
  const names = res.schema.elements.map((e) => e.name?.some ?? e.name);
  return res.rows.map((row) => Object.fromEntries(names.map((n, i) => [n, row[i]])));
}

/** Unwrap a SATS Result returned over HTTP: [0, ok] or [1, err]. */
export function unwrap(result) {
  const [tag, value] = result;
  if (tag !== 0) throw new Error(String(value));
  return value;
}
export const some = (v) => ({ some: v });
export const none = { none: [] };

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Poll until `fn` returns a truthy value (or throw after `ms`). */
export async function until(fn, ms = 10_000, label = 'condition') {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${label}`);
    await sleep(100);
  }
}

let passed = 0;
let failed = 0;
const only = process.env.ONLY;
export async function test(name, fn) {
  if (only && !name.includes(only)) return;
  try {
    await fn();
    passed++;
    console.log(`  ok    ${name}`);
  } catch (e) {
    failed++;
    console.log(`  FAIL  ${name}\n        ${String(e?.stack ?? e).split('\n').slice(0, 4).join('\n        ')}`);
  }
}
export function done() {
  console.log(`\n${passed} passed, ${failed} failed`);
  process.exit(failed ? 1 : 0);
}
