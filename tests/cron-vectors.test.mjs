// @spacetimedb/cron port: expression parsing, next-occurrence and time-zone parity with the
// real cron-parser 5.5.0 and IANA tzdata (vectors from tools/gen-cron-vectors.mjs and
// tools/gen-tz-vectors.py), checked through batch procedures.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { call, test, done } from './lib.mjs';

const cron = JSON.parse(readFileSync(new URL('./vectors/cron.json', import.meta.url)));
const tz = JSON.parse(readFileSync(new URL('./vectors/tz.json', import.meta.url)));
const opt = (o) => (o[0] === 0 ? o[1] : null);

await test(`parse acceptance matches cron-parser for ${cron.parseErrors.length} expressions`, async () => {
  const got = await call('cron_parse_batch', [cron.parseErrors.map((p) => p.expression)]);
  const wrong = cron.parseErrors.filter((p, i) => got[i] !== p.ok).map((p) => `${JSON.stringify(p.expression)} expected ok=${p.ok}`);
  assert.deepEqual(wrong, []);
});

await test(`time-zone names: ${cron.timezones.length} validity checks`, async () => {
  const got = await call('cron_valid_timezone_batch', [cron.timezones.map((t) => t.tz)]);
  const wrong = cron.timezones.filter((t, i) => got[i] !== t.valid).map((t) => `${JSON.stringify(t.tz.slice(0, 30))} expected ${t.valid}`);
  assert.deepEqual(wrong, []);
});

await test(`next occurrence matches cron-parser in ${cron.cases.length} cases (${cron.expressions.length} expressions × ${cron.zones.length} zones, DST edges, 3-step chains)`, async () => {
  const wrong = [];
  for (let i = 0; i < cron.cases.length; i += 2000) {
    const batch = cron.cases.slice(i, i + 2000);
    const got = await call('cron_next_batch', [batch.map(([e, z, after]) => [cron.expressions[e], Number(after), cron.zones[z]])]);
    batch.forEach(([e, z, after, next], k) => {
      const g = opt(got[k]);
      const have = g === null ? null : String(g);
      if (have !== next) wrong.push(`${cron.expressions[e]} [${cron.zones[z]}] after ${new Date(Number(BigInt(after) / 1000n)).toISOString()}: got ${have && new Date(Number(BigInt(have) / 1000n)).toISOString()}, expected ${next && new Date(Number(BigInt(next) / 1000n)).toISOString()}`);
    });
  }
  assert.deepEqual(wrong.slice(0, 20), [], `${wrong.length} mismatches`);
});

await test(`UTC offsets match tzdata in ${tz.cases.length} cases (every zone, both sides of every 2025-2031 transition)`, async () => {
  const wrong = [];
  for (let i = 0; i < tz.cases.length; i += 4000) {
    const batch = tz.cases.slice(i, i + 4000);
    const got = await call('cron_utc_offset_batch', [batch.map(([name, t]) => [name, t])]);
    batch.forEach(([name, t, off], k) => {
      if (opt(got[k]) !== off) wrong.push(`${name} at ${new Date(t * 1000).toISOString()}: got ${opt(got[k])}, expected ${off}`);
    });
  }
  assert.deepEqual(wrong.slice(0, 20), [], `${wrong.length} mismatches`);
});

done();
