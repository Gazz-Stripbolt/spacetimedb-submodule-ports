// The Rust and C# ports of a package must expose the same canonical schema: tables, columns,
// types, indexes, reducers, procedures and views. Compares two published databases.
//
//   node tests/schema-parity.mjs <rust-db> <csharp-db>
import assert from 'node:assert/strict';

const HOST = process.env.HOST ?? 'http://127.0.0.1:3000';
const [a, b] = process.argv.slice(2);

async function schema(db) {
  const r = await fetch(`${HOST}/v1/database/${db}/schema?version=9`);
  if (!r.ok) throw new Error(`${db}: HTTP ${r.status}`);
  return r.json();
}

/**
 * Function parameter names aren't canonicalized in 2.11 (Rust `max_failures`, C# `maxFailures`),
 * and only their order and types matter on the wire, so compare them in one style.
 */
const paramName = (n) => n.replace(/^_+/, '').replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`);
const params = (elements) => ({ elements: elements.map((e) => ({ ...e, name: e.name?.some ? { some: paramName(e.name.some) } : e.name })) });

/** A structural description of a type, resolving refs (type names don't matter, field names do). */
function describe(ty, typespace, seen = new Set()) {
  const [kind] = Object.keys(ty);
  const v = ty[kind];
  switch (kind) {
    case 'Ref':
      if (seen.has(v)) return `rec#${v}`;
      return describe(typespace.types[v], typespace, new Set([...seen, v]));
    case 'Product':
      return `{${v.elements.map((e) => `${e.name?.some ?? '_'}:${describe(e.algebraic_type, typespace, seen)}`).join(',')}}`;
    case 'Sum':
      return `<${v.variants.map((e) => `${e.name?.some ?? '_'}:${describe(e.algebraic_type, typespace, seen)}`).join('|')}>`;
    case 'Array':
      return `[${describe(v, typespace, seen)}]`;
    default:
      return kind;
  }
}

function summarize(s) {
  const out = {};
  const ts = s.typespace;
  for (const t of s.tables) {
    const row = ts.types[t.product_type_ref].Product.elements;
    const col = (i) => row[i].name.some;
    out[`table ${t.name}`] = {
      access: Object.keys(t.table_access)[0],
      type: Object.keys(t.table_type)[0],
      columns: describe({ Ref: t.product_type_ref }, ts),
      primaryKey: t.primary_key.map(col),
      indexes: t.indexes.map((i) => Object.values(i.algorithm)[0].map(col).join('+')).sort(),
      schedule: t.schedule?.some ? `${t.schedule.some.reducer_name}` : null,
    };
  }
  for (const r of s.reducers) {
    out[`reducer ${r.name}`] = describe({ Product: params(r.params.elements) }, ts);
  }
  for (const m of s.misc_exports) {
    const [kind] = Object.keys(m);
    const x = m[kind];
    if (!x?.name) continue;
    out[`${kind.toLowerCase()} ${x.name}`] = `${describe({ Product: params(x.params?.elements ?? []) }, ts)} -> ${x.return_type ? describe(x.return_type, ts) : ''}`;
  }
  return out;
}

const [sa, sb] = [summarize(await schema(a)), summarize(await schema(b))];
const keys = [...new Set([...Object.keys(sa), ...Object.keys(sb)])].sort();
const diffs = keys.filter((k) => JSON.stringify(sa[k]) !== JSON.stringify(sb[k]));
for (const k of diffs) console.log(`  ${k}\n    ${a}: ${JSON.stringify(sa[k])}\n    ${b}: ${JSON.stringify(sb[k])}`);
assert.equal(diffs.length, 0, `${diffs.length} schema differences between ${a} and ${b}`);
console.log(`  ok    ${a} and ${b} expose the same schema (${keys.length} tables, reducers, procedures and views)`);
