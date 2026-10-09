// Generate tests/vectors/cron.json: expected answers from the real cron-parser 5.5.0 (what
// @spacetimedb/cron uses), for the Rust and C# ports to match.
//
//   mkdir /tmp/cp && cd /tmp/cp && npm i cron-parser@5.5.0
//   NODE_PATH=/tmp/cp/node_modules node tools/gen-cron-vectors.mjs
//
// Node's ICU time-zone data should be the same tzdata release as tools/gen-tz.py used.
import { createRequire } from 'node:module';
import { writeFileSync } from 'node:fs';
const require = createRequire(import.meta.url);
const { CronExpressionParser } = require('cron-parser');

const HASH_SEED = 'spacetimedb-cron-submodule';
const MAX_SAFE_DATE_MS = 8_640_000_000_000_000;

// parser.ts at the pinned commit, verbatim in behaviour.
function parseCronExpression(expr) {
  const expression = expr.trim();
  if (expression.length === 0) throw new Error('cron expression is empty');
  if (expression.length > 256) throw new Error('cron expression exceeds 256 characters');
  CronExpressionParser.parse(expression, { hashSeed: HASH_SEED });
  return { expression };
}
function isValidTimezone(tz) {
  if (tz.length === 0 || tz.length > 128 || tz !== tz.trim()) return false;
  if (tz === 'UTC') return true;
  try {
    new Intl.DateTimeFormat('en-US', { timeZone: tz });
    return true;
  } catch {
    return false;
  }
}
function nextFireAfter(parsed, afterMicros, timezone = 'UTC') {
  if (!isValidTimezone(timezone)) return undefined;
  const afterMs = Number(afterMicros / 1000n);
  if (!Number.isFinite(afterMs) || Math.abs(afterMs) > MAX_SAFE_DATE_MS) return undefined;
  try {
    const interval = CronExpressionParser.parse(parsed.expression, { currentDate: new Date(afterMs), tz: timezone, hashSeed: HASH_SEED });
    return BigInt(interval.next().getTime()) * 1000n;
  } catch {
    return undefined;
  }
}

const valid = [
  '* * * * *', '*/15 * * * *', '0 9 * * *', '0 9 * * 1', '0 9 * * MON', '0 0 1 * *', '15 9 * * 1-5',
  '0-30/5 * * * *', '0 0 1 * 1', '0 0 1 JAN *', 'H * * * *', 'H H * * *', 'H(0-29)/10 * * * *', 'H/20 * * * *',
  '*/2 * * * * *', '30 2 * * *', '30 1 * * *', '0 * * * *', '0 0 L * *', '0 0 * * 5L', '0 0 * * 1#2',
  '0 0 * * MON#1', '0 0 29 2 *', '@daily', '@hourly', '@weekly', '@monthly', '@yearly', '@weekdays', '@weekends',
  '0 0 * * 7', '0 0 * * 0-7', '0 12 * * SUN,SAT', '5 4 * * sun', '0 22 * * 1-5', '23 0-20/2 * * *',
  '0 0,12 1 */2 *', '0 0 1-7 * 1', '0 0 15 * FRI', '0 0 ? * MON', '0 0 L 2 *', '0 0 5L * *', '*/7 * * * *',
  '59 23 31 12 *', '0 0 * * 6L', '10 * * * * *', '0 0 1,15 * 3', '*/1 * * * *', '0 0 */1 * 1', 'H(10-20) * * * *',
  '0 0 * * L', '0 30 2 * * *', '0 0 2-3 * * *', '*/20 * 1-3 * * *', '0 0 31 * *', '0 0 * JUL-AUG *', '0 0 1 1,7 *',
  '30 * * * *', '0 */3 * * *', '45 23 * * 0', '0 6 * * 1#5', '0 0 1 * MON-FRI', '0 0 0 1 1 *', 'h * * * *',
  '7/10 * * * *', '0 0 ? * *', '0 0 * * ?', '*/30 */2 * * * *', '0 15 10 ? * MON-FRI',
];
const invalid = [
  'invalid', '0 0 31 2 *', '0 0 30 2 *', '60 * * * *', '* 24 * * *', '* * 32 * *', '* * * 13 *', '* * * * 8',
  '* * * * * * *', '1,1 * * * *', '5-1 * * * *', '*/0 * * * *', '0 0 * * MON#6', '0 0 * * 1#2,3', 'MONDAY * * * *',
  'H(5-1) * * * *', '0 0 L * * extra x', '* * * * FOO', '1-60 * * * *', '0 0 0 * * 1#0', ',1 * * * *', '1/2/3 * * * *',
];
const zones = [
  'UTC', 'America/New_York', 'America/Los_Angeles', 'Europe/London', 'Europe/Berlin', 'Australia/Sydney', 'Asia/Tokyo',
  'Asia/Kolkata', 'Pacific/Chatham', 'Australia/Lord_Howe', 'America/Sao_Paulo', 'Africa/Casablanca', 'Europe/Dublin',
  'America/St_Johns', 'America/Santiago', 'Pacific/Auckland', 'Asia/Gaza', 'america/chicago',
];
const iso = (s) => BigInt(Date.parse(s)) * 1000n;
const starts = [
  '2026-01-15T00:00:00Z', '2026-03-08T06:59:59Z', '2026-03-08T07:30:00Z', '2026-03-29T00:59:00Z', '2026-04-05T15:59:30Z',
  '2026-05-04T12:00:30.500Z', '2026-10-25T00:30:00Z', '2026-11-01T05:30:00Z', '2026-11-01T06:15:00Z', '2027-02-27T23:59:59Z',
  '2028-02-28T12:00:00Z', '2029-12-31T23:59:59Z',
].map(iso);

const parses = (e) => {
  try {
    parseCronExpression(e);
    return true;
  } catch {
    return false;
  }
};
const expressions = valid.filter(parses);
const exprIndex = new Map(expressions.map((e, i) => [e, i]));
const cases = []; // [expression index, zone index, after µs, next µs | null]
for (const expression of expressions) {
  const parsed = parseCronExpression(expression);
  for (const timezone of zones) {
    for (const start of starts) {
      let after = start;
      for (let step = 0; step < 3; step++) {
        const next = nextFireAfter(parsed, after, timezone);
        cases.push([exprIndex.get(expression), zones.indexOf(timezone), String(after), next === undefined ? null : String(next)]);
        if (next === undefined) break;
        after = next;
      }
    }
  }
}
// Whether each expression parses at all (the valid list can contain surprises, like 'h').
const parseErrors = [...valid, ...invalid].map((expression) => ({ expression, ok: parses(expression) }));
const tzNames = ['UTC', 'America/New_York', 'america/new_york', 'US/Eastern', 'Etc/GMT+5', 'EST5EDT', 'Not_A_Real/Timezone',
  ' UTC', 'utc', 'Asia/Calcutta', 'Mars/Olympus_Mons', '', 'x'.repeat(129), 'Europe/Kyiv', 'Europe/Kiev', 'GMT', 'Etc/UTC'];
const timezones = tzNames.map((tz) => ({ tz, valid: isValidTimezone(tz) }));
writeFileSync(new URL('../tests/vectors/cron.json', import.meta.url),
  JSON.stringify({ generatedWith: 'cron-parser 5.5.0', tzdata: process.versions.tz, expressions, zones, cases, parseErrors, timezones }));
console.log(`${cases.length} cases, ${parseErrors.length} parse checks, ${timezones.length} zone checks (tzdata ${process.versions.tz})`);
