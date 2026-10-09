// Generate tests/vectors/grid.json: A* and Dijkstra answers from the real @spacetimedb/grid
// math (src/math at the pinned commit), for the Rust and C# ports to match exactly: paths,
// costs, expansion counts and the order of reachable cells.
//
//   UPSTREAM=<SpacetimeDB checkout at f6ed7dc>/spacetime-grid-ts npx tsx tools/gen-grid-vectors.mjs
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const upstream = process.env.UPSTREAM;
if (!upstream) throw new Error('set UPSTREAM to the spacetime-grid-ts package directory');
const { neighbors, distance, findPathAstar, dijkstra, coordKey } = await import(pathToFileURL(join(upstream, 'src/math/index.ts')).href);

// Deterministic PRNG (mulberry32), so the vectors are reproducible.
let seed = 0x5eed;
const rand = () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const int = (n) => Math.floor(rand() * n);

const cost = (rows) => (c) => {
  const ch = rows[c.y]?.[c.x];
  if (ch === '#') return -1;
  if (ch >= '2' && ch <= '9') return Number(ch);
  return 1;
};

const kinds = [['square', 4], ['square', 8], ['hex', 6]];
const cases = [];
for (let i = 0; i < 240; i++) {
  const [kind, connectivity] = kinds[i % 3];
  const w = 3 + int(12);
  const h = 3 + int(12);
  const wallP = rand() * 0.35;
  const rows = Array.from({ length: h }, () =>
    Array.from({ length: w }, () => (rand() < wallP ? '#' : rand() < 0.2 ? String(2 + int(8)) : '.')).join('')
  );
  const start = { x: int(w), y: int(h) };
  const goal = { x: int(w), y: int(h) };
  const inBounds = (c) => c.x >= 0 && c.y >= 0 && c.x < w && c.y < h;
  const nb = (c) => neighbors(kind, c, connectivity).filter(inBounds);
  const useHeuristic = i % 5 !== 4;
  const a = findPathAstar({
    start,
    goal,
    cost: cost(rows),
    neighbors: nb,
    heuristic: useHeuristic ? (p, q) => distance(kind, p, q, connectivity) : undefined,
  });
  const maxCost = int(20) - 1;
  const d = dijkstra({ start, cost: cost(rows), neighbors: nb, maxCost });
  cases.push({
    rows,
    kind,
    connectivity,
    start,
    goal,
    useHeuristic,
    astar: a.found ? { found: true, cells: a.cells.map((c) => [c.x, c.y]), cost: a.cost, expanded: a.expanded } : { found: false, cells: [], cost: 0, expanded: a.expanded },
    maxCost,
    dijkstra: [...d.values()].map((n) => [n.cell.x, n.cell.y, n.cost]),
  });
  void coordKey;
}
writeFileSync(new URL('../tests/vectors/grid.json', import.meta.url), JSON.stringify({ commit: 'f6ed7dcd1c8f853c97fc7e9f126606fca7adadac', cases }) + '\n');
console.log(`wrote ${cases.length} cases`);
