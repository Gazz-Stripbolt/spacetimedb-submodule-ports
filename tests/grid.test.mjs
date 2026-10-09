// @spacetimedb/grid port: the upstream pathfinding and procedure tests (scripts/test-pathfind.ts
// and scripts/test-procedures.mjs at the pinned commit), 240 A*/Dijkstra vectors generated from
// the real TS math (tests/vectors/grid.json), and every helper end to end.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { call, callErr, procErr, sql, newIdentity, test, done, unwrap, some, none } from './lib.mjs';

const vectors = JSON.parse(readFileSync(new URL('./vectors/grid.json', import.meta.url), 'utf8'));
const opt = (v) => (v === undefined ? none : some(v));
const P = (x, y) => [x, y];
const astar = async (rows, start, goal, { kind = 'square', connectivity = 4, heuristic = true, bounded = true } = {}) => {
  const [found, cells, cost, expanded] = await call('demo_astar', [rows, P(start.x, start.y), P(goal.x, goal.y), kind, connectivity, heuristic, bounded]);
  return { found, cells, cost: Number(cost), expanded: Number(expanded) };
};
const dijkstra = async (rows, start, maxCost, kind = 'square', connectivity = 4) =>
  (await call('demo_dijkstra', [rows, P(start.x, start.y), maxCost, kind, connectivity])).map(([x, y, cost]) => [x, y, cost]);

const gridArgs = (o = {}) => {
  const a = { name: 'Arena', kind: 'square', orientation: 'flat', width: 5, height: 5, defaultCost: 1, connectivity: 4, mode: 'collaborative', ...o };
  return [a.name, a.kind, a.orientation, a.width, a.height, a.defaultCost, a.connectivity, a.mode];
};
const createGrid = async (owner, o) => Number(unwrap(await call('demo_create_grid', [owner, gridArgs(o)])));
const createErr = (owner, o) => procErr('demo_create_grid', [owner, gridArgs(o)]);
const place = async (owner, gridId, x, y, o = {}) =>
  Number(unwrap(await call('demo_place_entity', [owner, [gridId, x, y, o.kind ?? 'unit', o.blocks ?? true, opt(o.label)]])));
const placeErr = (owner, gridId, x, y, o = {}) => procErr('demo_place_entity', [owner, [gridId, x, y, o.kind ?? 'unit', o.blocks ?? true, opt(o.label)]]);
const setCost = (owner, gridId, x, y, cost, terrain) => call('demo_set_cell_cost', [owner, [gridId, x, y, cost, opt(terrain)]]);
const setCostErr = (owner, gridId, x, y, cost, terrain) => callErr('demo_set_cell_cost', [owner, [gridId, x, y, cost, opt(terrain)]]);
const move = (owner, id, x, y) => call('demo_move_entity', [owner, [id, x, y]]);
const moveErr = (owner, id, x, y) => callErr('demo_move_entity', [owner, [id, x, y]]);
const pathArgs = (gridId, s, e, storeFor, maxExp) => [gridId, s[0], s[1], e[0], e[1], opt(storeFor), opt(maxExp)];
const path = async (owner, ...a) => {
  const [found, cells, cost, expanded] = unwrap(await call('demo_compute_path', [owner, pathArgs(...a)]));
  return { found, cells, cost, expanded };
};
const pathErr = (owner, ...a) => procErr('demo_compute_path', [owner, pathArgs(...a)]);
const range = async (owner, gridId, x, y, maxCost) => unwrap(await call('demo_cells_in_range', [owner, [gridId, x, y, maxCost]]))[0];
const rows = (table, gridId) => sql(`SELECT * FROM ${table} WHERE grid_id = ${gridId}`);

// -- scripts/test-pathfind.ts ----------------------------------------------------------------

await test('A*: upstream cases (trivial, straight, around, unreachable, detour, weighted, hex)', async () => {
  let r = await astar(['..'], { x: 0, y: 0 }, { x: 0, y: 0 });
  assert.ok(r.found && r.cells.length === 1 && r.cost === 0, 'start equals goal');
  r = await astar(['.....'], { x: 0, y: 0 }, { x: 4, y: 0 });
  assert.ok(r.found && r.cost === 4 && r.cells.length === 5, 'straight line');
  r = await astar(['.....', '...#.', '...#.', '.....'], { x: 0, y: 0 }, { x: 4, y: 0 });
  assert.ok(r.found && r.cost === 4, 'no obstacle on the way');
  r = await astar(['...#.', '...#.', '...#.', '...#.'], { x: 0, y: 0 }, { x: 4, y: 0 });
  assert.equal(r.found, false, 'goal unreachable');
  r = await astar(['..#..', '..#..', '..#..', '.....'], { x: 0, y: 0 }, { x: 4, y: 0 });
  assert.ok(r.found && r.cost === 10, `forced detour costs 10 (got ${r.cost})`);
  r = await astar(['.....', '.333.', '.333.', '.....'], { x: 0, y: 0 }, { x: 4, y: 3 });
  assert.ok(r.found && r.cost === 7, `weighted terrain picks the cheap route (got ${r.cost})`);
  r = await astar(['.'], { x: 0, y: 0 }, { x: 3, y: 0 }, { kind: 'hex', bounded: false });
  assert.ok(r.found && r.cost === 3 && r.cells.length === 4, 'hex 3 cells apart');
});

await test('Dijkstra: upstream cases (radius, range 0, costs, walls)', async () => {
  const open5 = ['.....', '.....', '.....', '.....', '.....'];
  assert.equal((await dijkstra(open5, { x: 2, y: 2 }, 2)).length, 13, 'radius 2 on open 5x5');
  assert.equal((await dijkstra(['.'], { x: 0, y: 0 }, 0)).length, 1, 'range 0 is the start only');
  const corner = (await dijkstra(['...', '...', '...'], { x: 0, y: 0 }, 4)).find(([x, y]) => x === 2 && y === 2);
  assert.ok(corner && corner[2] === 4, 'corner cost 4');
  assert.equal((await dijkstra(['...', '###', '...'], { x: 0, y: 0 }, 100)).length, 3, 'a wall blocks the lower half');
});

await test(`vectors: ${vectors.cases.length} random maps match the TS A* and Dijkstra exactly (paths, costs, expansions, order)`, async () => {
  for (const [i, c] of vectors.cases.entries()) {
    const a = await astar(c.rows, c.start, c.goal, { kind: c.kind, connectivity: c.connectivity, heuristic: c.useHeuristic });
    assert.deepEqual(a, { found: c.astar.found, cells: c.astar.cells, cost: c.astar.cost, expanded: c.astar.expanded }, `case ${i} A*`);
    assert.deepEqual(await dijkstra(c.rows, c.start, c.maxCost, c.kind, c.connectivity), c.dijkstra, `case ${i} Dijkstra`);
  }
});

// -- scripts/test-procedures.mjs -------------------------------------------------------------

await test('upstream: only the owner deletes a grid', async () => {
  const gridId = await createGrid('alice');
  assert.equal(await callErr('demo_delete_grid', ['bob', gridId]), 'grid.not_owner');
  await call('demo_delete_grid', ['alice', gridId]);
  assert.equal((await sql(`SELECT * FROM grid WHERE id = ${gridId}`)).length, 0);
});

await test('upstream: terrain and blocking entities block moves and paths; invalid cost', async () => {
  const gridId = await createGrid('alice');
  const mover = await place('alice', gridId, 1, 1);
  await setCost('alice', gridId, 2, 1, 0, 'rock');
  assert.equal(await moveErr('alice', mover, 2, 1), 'grid.move_blocked');
  // A painted cell under a blocking entity still blocks.
  await setCost('bob', gridId, 1, 2, 3, 'mud');
  await place('bob', gridId, 1, 2, { kind: 'wall' });
  assert.equal(await moveErr('alice', mover, 1, 2), 'grid.move_blocked');
  assert.equal((await path('alice', gridId, [1, 1], [1, 2])).found, false);
  await move('alice', mover, 1, 0);
  assert.equal((await sql(`SELECT * FROM grid_entity WHERE id = ${mover}`))[0].y, 0);
  assert.equal(await setCostErr('alice', gridId, 0, 0, 10_001), 'grid.invalid_cost');
});

// -- the rest, end to end --------------------------------------------------------------------

await test('create: validation, hex stores connectivity 6', async () => {
  const cases = [
    [{ name: '' }, 'grid.invalid_name'],
    [{ name: 'n'.repeat(129) }, 'grid.invalid_name'],
    [{ kind: 'tri' }, 'grid.invalid_kind'],
    [{ orientation: 'diagonal' }, 'grid.invalid_orientation'],
    [{ mode: 'public' }, 'grid.invalid_mode'],
    [{ width: 0 }, 'grid.invalid_dimensions'],
    [{ height: 1025 }, 'grid.invalid_dimensions'],
    [{ defaultCost: 0 }, 'grid.invalid_default_cost'],
    [{ defaultCost: 10_001 }, 'grid.invalid_default_cost'],
    [{ connectivity: 6 }, 'grid.invalid_connectivity'],
  ];
  for (const [o, code] of cases) assert.equal(await createErr('alice', o), code, JSON.stringify(o));
  const hex = await createGrid('alice', { kind: 'hex', orientation: 'pointy', connectivity: 99 });
  assert.equal((await sql(`SELECT * FROM grid WHERE id = ${hex}`))[0].connectivity, 6);
});

await test('modes: owner grids are owner-only; collaborative grids are shared, entities stay their owner\'s', async () => {
  const mine = await createGrid('alice', { mode: 'owner' });
  assert.equal(await setCostErr('bob', mine, 0, 0, 3), 'grid.not_owner');
  assert.equal(await placeErr('bob', mine, 0, 0), 'grid.not_owner');
  assert.equal(await pathErr('bob', mine, [0, 0], [1, 1]), 'grid.not_owner');
  assert.equal(await procErr('demo_cells_in_range', ['bob', [mine, 0, 0, 2]]), 'grid.not_owner');
  const shared = await createGrid('alice');
  await setCost('bob', shared, 0, 0, 3);
  const bobs = await place('bob', shared, 4, 4);
  assert.equal(await moveErr('alice', bobs, 3, 4), 'grid.entity_not_owner');
  assert.equal(await moveErr('alice', 999_999, 3, 4), 'grid.entity_not_found');
  await move('bob', bobs, 3, 4);
});

await test('cells: default cost without terrain removes the row; terrain keeps it; bounds and limits', async () => {
  const g = await createGrid('alice', { defaultCost: 2 });
  await setCost('alice', g, 1, 1, 5, 'mud');
  await setCost('alice', g, 1, 1, 2, 'grass');
  let cells = await rows('cell_state', g);
  assert.deepEqual(cells.map((c) => [c.x, c.y, c.cost]), [[1, 1, 2]], 'terrain keeps a default-cost row');
  await setCost('alice', g, 1, 1, 2);
  assert.equal((await rows('cell_state', g)).length, 0, 'default cost and no terrain removes it');
  assert.equal(await setCostErr('alice', g, 5, 0, 3), 'grid.out_of_bounds');
  assert.equal(await setCostErr('alice', g, -1, 0, 3), 'grid.out_of_bounds');
  assert.equal(await setCostErr('alice', g, 0, 0, 3, 't'.repeat(65)), 'grid.invalid_terrain');
  assert.equal(await callErr('demo_paint_cells', ['alice', [g, Array.from({ length: 1001 }, () => [0, 0, 3, none])]]), 'grid.too_many_cells');
  await call('demo_paint_cells', ['alice', [g, [[0, 0, -1, some('wall')], [1, 0, 7, none], [2, 0, 2, none]]]]);
  cells = await rows('cell_state', g);
  assert.deepEqual(cells.map((c) => [c.x, c.y, c.cost]).sort(), [[0, 0, -1], [1, 0, 7]]);
  assert.equal(await callErr('demo_paint_cells', ['alice', [g, [[0, 1, 3, none], [9, 9, 3, none]]]]), 'grid.out_of_bounds');
  assert.equal((await rows('cell_state', g)).length, 2, 'a failing paint writes nothing');
});

await test('entities: validation, adjacency (square 4/8, hex), bounds', async () => {
  const g = await createGrid('alice');
  assert.equal(await placeErr('alice', g, 0, 0, { kind: '' }), 'grid.invalid_entity_kind');
  assert.equal(await placeErr('alice', g, 0, 0, { kind: 'k'.repeat(65) }), 'grid.invalid_entity_kind');
  assert.equal(await placeErr('alice', g, 0, 0, { label: 'l'.repeat(257) }), 'grid.invalid_entity_label');
  assert.equal(await placeErr('alice', g, 5, 5), 'grid.out_of_bounds');
  assert.equal(await placeErr('alice', 999_999, 0, 0), 'grid.not_found');
  const u = await place('alice', g, 2, 2, { label: 'hero' });
  assert.equal(await moveErr('alice', u, 3, 3), 'grid.move_not_adjacent', 'no diagonals with 4-connectivity');
  assert.equal(await moveErr('alice', u, 2, 4), 'grid.move_not_adjacent');
  assert.equal(await moveErr('alice', u, 2, 5), 'grid.out_of_bounds');
  const g8 = await createGrid('alice', { connectivity: 8 });
  const u8 = await place('alice', g8, 2, 2);
  await move('alice', u8, 3, 3);
  const hex = await createGrid('alice', { kind: 'hex' });
  const h = await place('alice', hex, 2, 2);
  await move('alice', h, 3, 1);
  assert.equal(await moveErr('alice', h, 4, 2), 'grid.move_not_adjacent', 'hex (1,1) is not a neighbour');
  // Non-blocking entities don't block; blocking ones do (but not the mover itself).
  await place('bob', g, 2, 1, { blocks: false, kind: 'coin' });
  await move('alice', u, 2, 1);
});

await test('paths: store_for writes entity_path; ownership, grid mismatch, max_expansions', async () => {
  const g = await createGrid('alice', { width: 6, height: 4 });
  await call('demo_paint_cells', ['alice', [g, [[2, 0, -1, none], [2, 1, -1, none], [2, 2, -1, none]]]]);
  const u = await place('alice', g, 0, 0);
  const r = await path('alice', g, [0, 0], [4, 0], u);
  assert.equal(r.found, true);
  assert.equal(r.cost, 10);
  assert.deepEqual(r.cells[0], [0, 0]);
  assert.deepEqual(r.cells.at(-1), [4, 0]);
  const stored = (await sql(`SELECT * FROM entity_path WHERE entity_id = ${u}`))[0];
  assert.equal(stored.cost, 10);
  assert.equal(stored.cells.length, r.cells.length);
  const other = await createGrid('alice');
  const elsewhere = await place('alice', other, 0, 0);
  assert.equal(await pathErr('alice', g, [0, 0], [4, 0], elsewhere), 'grid.entity_grid_mismatch');
  const bobs = await place('bob', g, 5, 3);
  assert.equal(await pathErr('alice', g, [0, 0], [4, 0], bobs), 'grid.entity_not_owner');
  assert.equal(await pathErr('alice', g, [0, 0], [4, 0], 999_999), 'grid.entity_not_found');
  assert.equal(await pathErr('alice', g, [0, 0], [6, 0]), 'grid.out_of_bounds');
  assert.equal(await pathErr('alice', g, [0, 0], [4, 0], undefined, 0), 'grid.invalid_max_expansions');
  assert.equal(await pathErr('alice', g, [0, 0], [4, 0], undefined, 50_001), 'grid.invalid_max_expansions');
  const capped = await path('alice', g, [0, 0], [4, 0], undefined, 3);
  assert.deepEqual(capped, { found: false, cells: [], cost: 0, expanded: 3 });
});

await test('range: Dijkstra over the cost map (blocking entities count as walls); too large is refused', async () => {
  const g = await createGrid('alice');
  await setCost('alice', g, 1, 0, 3);
  await place('alice', g, 0, 1);
  const cells = await range('alice', g, 0, 0, 3);
  assert.deepEqual(cells, [[0, 0, 0], [1, 0, 3]], 'the blocking entity at (0,1) walls off the rest');
  assert.equal(await procErr('demo_cells_in_range', ['alice', [g, 9, 0, 3]]), 'grid.out_of_bounds');
  const big = await createGrid('alice', { width: 100, height: 100 });
  assert.equal(await procErr('demo_cells_in_range', ['alice', [big, 50, 50, 60]]), 'grid.range_too_large');
  assert.equal((await range('alice', big, 50, 50, 10)).length, 221);
});

await test('delete cascades cells, entities and stored paths', async () => {
  const g = await createGrid('alice');
  await setCost('alice', g, 1, 1, 4);
  const u = await place('alice', g, 0, 0);
  await path('alice', g, [0, 0], [4, 4], u);
  assert.equal((await rows('entity_path', g)).length, 1);
  assert.equal(await callErr('demo_delete_grid', ['alice', 999_999]), 'grid.not_found');
  await call('demo_delete_grid', ['alice', g]);
  for (const t of ['cell_state', 'grid_entity', 'entity_path']) assert.equal((await rows(t, g)).length, 0, t);
});

await test('README host operations: the caller\'s identity is the owner', async () => {
  const alice = await newIdentity();
  const gridId = Number(unwrap(await call('create_player_grid', [gridArgs({ mode: 'owner' })], alice)));
  const g = (await sql(`SELECT * FROM grid WHERE id = ${gridId}`))[0];
  assert.equal(g.owner_user_id, alice.identity.toLowerCase().replace(/^0x/, ''));
  const u = await place(g.owner_user_id, gridId, 0, 0);
  await call('move_my_entity', [[u, 1, 0]], alice);
  const bob = await newIdentity();
  assert.equal(await callErr('move_my_entity', [[u, 2, 0]], bob), 'grid.entity_not_owner');
});

done();
