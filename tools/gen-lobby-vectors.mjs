// Generate tests/vectors/lobby.json: ranked matchmaking and Elo answers from the real
// @spacetimedb/lobby code (src/matchmaking.ts and src/keys.ts at the pinned commit).
//
//   UPSTREAM=<SpacetimeDB checkout at f6ed7dc>/spacetime-lobby-ts npx tsx tools/gen-lobby-vectors.mjs
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const upstream = process.env.UPSTREAM;
if (!upstream) throw new Error('set UPSTREAM to the spacetime-lobby-ts package directory');
const mm = await import(pathToFileURL(join(upstream, 'src/matchmaking.ts')).href);
const { lobbyCompositeKey } = await import(pathToFileURL(join(upstream, 'src/keys.ts')).href);

let seed = 0x10bb;
const rand = () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const int = (n) => Math.floor(rand() * n);

const selections = [];
for (let i = 0; i < 300; i++) {
  const n = 1 + int(14);
  const pools = ['ranked', 'ranked', 'casual', undefined];
  let t = BigInt(int(1000)) * 1_000_000n;
  const tickets = Array.from({ length: n }, (_, k) => {
    t += BigInt(int(15_000_000)); // oldest first, as the index returns them
    return {
      ticketId: `t${k}`,
      rating: rand() < 0.15 ? undefined : 600 + int(1000),
      ratingPool: pools[int(pools.length)],
      createdAt: { microsSinceUnixEpoch: t },
    };
  });
  const matchSize = 2 + int(3);
  const now = t + BigInt(int(120_000_000));
  const picked = mm.rankedSelection(tickets, matchSize, now);
  selections.push({
    tickets: tickets.map((x) => ({ ticketId: x.ticketId, rating: x.rating ?? null, ratingPool: x.ratingPool ?? null, createdAt: x.createdAt.microsSinceUnixEpoch.toString() })),
    matchSize,
    now: now.toString(),
    picked: picked ? picked.map((x) => x.ticketId) : null,
  });
}

const elo = [];
for (let i = 0; i < 300; i++) {
  const rating = 100 + int(4901);
  const opponent = 100 + int(4901);
  const score = [0, 0.5, 1][int(3)];
  const e = mm.expectedScore(rating, opponent);
  elo.push({ rating, opponent, score, expected: e, next: mm.updatedRating(rating, e, score) });
}

const bands = [];
for (let i = 0; i < 50; i++) {
  const created = BigInt(int(1_000_000_000));
  const now = created + BigInt(int(400_000_000)) - 50_000_000n;
  bands.push({ created: created.toString(), now: now.toString(), band: mm.rankedBand({ createdAt: { microsSinceUnixEpoch: created } }, now) });
}

const keys = [['ranked', 'player-1'], ['a:b', 'c'], ['a', 'b:c'], ['é😀', ''], ['x']].map((parts) => ({ parts, key: lobbyCompositeKey(...parts) }));

writeFileSync(new URL('../tests/vectors/lobby.json', import.meta.url), JSON.stringify({ commit: 'f6ed7dcd1c8f853c97fc7e9f126606fca7adadac', selections, elo, bands, keys }) + '\n');
console.log(`wrote ${selections.length} selections (${selections.filter((s) => s.picked).length} matched), ${elo.length} Elo, ${bands.length} bands`);
