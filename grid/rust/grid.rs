//! # grid.rs: square and hex maps, units, obstacles, pathfinding and movement ranges
//!
//! Unofficial Rust port of `@spacetimedb/grid` (SpacetimeDB PR #5908, commit
//! `f6ed7dcd1c8f853c97fc7e9f126606fca7adadac`, package `spacetime-grid-ts`). Licensed
//! Apache-2.0, like the original. Original design and behaviour by Clockwork Labs; this port
//! is by Tinker (Gazz-Stripbolt).
//!
//! Same private tables (`grid`, `cell_state`, `grid_entity`, `entity_path`), limits, error
//! codes and algorithms as the TS package. A* and Dijkstra are ported step for step (same
//! binary heap, same neighbour order, same tie-breaking), so they return the same paths, costs,
//! expansion counts and cell order as TypeScript.
//!
//! Like the TS package, it defines no reducers: your module exposes its own operations and
//! passes the owner it trusts (an identity, a user id, any string):
//!
//! ```ignore
//! #[spacetimedb::procedure]
//! pub fn create_player_grid(ctx: &mut ProcedureContext, args: grid::CreateGridArgs) -> Result<u64, String> {
//!     let owner = ctx.sender().to_hex().to_string();
//!     ctx.try_with_tx(|tx| grid::create_grid(tx, &args, &owner))
//! }
//!
//! #[spacetimedb::reducer]
//! pub fn move_my_entity(ctx: &ReducerContext, args: grid::MoveEntityArgs) -> Result<(), String> {
//!     grid::move_entity(ctx, &args, &ctx.sender().to_hex())
//! }
//! ```
//!
//! Changes from the TypeScript original:
//! - The `*Params` objects become argument structs (`CreateGridArgs`, ...). They derive
//!   `SpacetimeType`, so a host can take one as a single reducer or procedure argument, or
//!   declare the fields itself.
//! - Errors are `Err(String)` with the same `grid.*` codes.
//! - The math helpers take closures and `Coord` values, like the TS ones, and are public.

use spacetimedb::{ReducerContext, SpacetimeType, Table, Timestamp};
use std::collections::HashMap;

pub const GRID_KIND_SQUARE: &str = "square";
pub const GRID_KIND_HEX: &str = "hex";
pub const GRID_ORIENTATION_FLAT: &str = "flat";
pub const GRID_ORIENTATION_POINTY: &str = "pointy";
pub const GRID_MODE_OWNER: &str = "owner";
pub const GRID_MODE_COLLABORATIVE: &str = "collaborative";

const GRID_MAX_DIM: i32 = 1024;
/// Path costs stay inside i32: at most PATH_MAX_EXPANSIONS steps of MAX_CELL_COST.
const MAX_CELL_COST: i32 = 10_000;
const PATH_MAX_EXPANSIONS: i32 = 50_000;
const MAX_RESULT_CELLS: usize = 5_000;
const MAX_PAINT_CELLS: usize = 1_000;
const MAX_NAME_LENGTH: usize = 128;
const MAX_KIND_LENGTH: usize = 64;
const MAX_LABEL_LENGTH: usize = 256;
const MAX_TERRAIN_LENGTH: usize = 64;
const DEFAULT_MAX_EXPANSIONS: i32 = 100_000;

/// The `grid.*` codes.
pub mod errors {
    pub const INVALID_NAME: &str = "grid.invalid_name";
    pub const INVALID_KIND: &str = "grid.invalid_kind";
    pub const INVALID_ORIENTATION: &str = "grid.invalid_orientation";
    pub const INVALID_MODE: &str = "grid.invalid_mode";
    pub const INVALID_DIMENSIONS: &str = "grid.invalid_dimensions";
    pub const INVALID_DEFAULT_COST: &str = "grid.invalid_default_cost";
    pub const INVALID_CONNECTIVITY: &str = "grid.invalid_connectivity";
    pub const INVALID_COST: &str = "grid.invalid_cost";
    pub const INVALID_TERRAIN: &str = "grid.invalid_terrain";
    pub const TOO_MANY_CELLS: &str = "grid.too_many_cells";
    pub const INVALID_ENTITY_KIND: &str = "grid.invalid_entity_kind";
    pub const INVALID_ENTITY_LABEL: &str = "grid.invalid_entity_label";
    pub const INVALID_MAX_EXPANSIONS: &str = "grid.invalid_max_expansions";
    pub const NOT_FOUND: &str = "grid.not_found";
    pub const NOT_OWNER: &str = "grid.not_owner";
    pub const ENTITY_NOT_FOUND: &str = "grid.entity_not_found";
    pub const ENTITY_NOT_OWNER: &str = "grid.entity_not_owner";
    pub const ENTITY_GRID_MISMATCH: &str = "grid.entity_grid_mismatch";
    pub const OUT_OF_BOUNDS: &str = "grid.out_of_bounds";
    pub const MOVE_NOT_ADJACENT: &str = "grid.move_not_adjacent";
    pub const MOVE_BLOCKED: &str = "grid.move_blocked";
    pub const PATH_TOO_LONG: &str = "grid.path_too_long";
    pub const RANGE_TOO_LARGE: &str = "grid.range_too_large";
}

// ---------------------------------------------------------------------------
// Tables and types
// ---------------------------------------------------------------------------

/// `owner_user_id` is opaque: an identity, an application user id, or a host-defined actor
/// id. Owner mode restricts access to that value.
#[spacetimedb::table(accessor = grid)]
#[derive(Clone, Debug)]
pub struct Grid {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub owner_user_id: String,
    pub name: String,
    /// `"square"` or `"hex"`.
    pub kind: String,
    /// `"flat"` or `"pointy"` (ignored for square).
    pub orientation: String,
    pub width: i32,
    pub height: i32,
    pub default_cost: i32,
    /// Square only: 4 or 8. Hex is always 6.
    pub connectivity: i32,
    /// `"owner"` or `"collaborative"`.
    pub mode: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Sparse: only non-default cells have a row. `cost <= 0` means blocked.
#[spacetimedb::table(
    accessor = cell_state,
    index(accessor = by_grid_cell, btree(columns = [grid_id, x, y]))
)]
#[derive(Clone, Debug)]
pub struct CellState {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub grid_id: u64,
    pub x: i32,
    pub y: i32,
    pub cost: i32,
    pub terrain: Option<String>,
}

#[spacetimedb::table(
    accessor = grid_entity,
    index(accessor = by_grid_cell, btree(columns = [grid_id, x, y]))
)]
#[derive(Clone, Debug)]
pub struct GridEntity {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub grid_id: u64,
    #[index(btree)]
    pub owner_user_id: String,
    pub x: i32,
    pub y: i32,
    /// User-defined: `"player"`, `"npc"`, `"item"`, ...
    pub kind: String,
    pub blocks_movement: bool,
    pub label: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Not auto-invalidated; call `compute_path` again for a fresh route.
#[spacetimedb::table(accessor = entity_path)]
#[derive(Clone, Debug)]
pub struct EntityPath {
    #[primary_key]
    pub entity_id: u64,
    #[index(btree)]
    pub grid_id: u64,
    pub cells: Vec<PathCell>,
    pub cost: i32,
    pub computed_at: Timestamp,
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathCell {
    pub x: i32,
    pub y: i32,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct PathResult {
    pub found: bool,
    pub cells: Vec<PathCell>,
    pub cost: i32,
    pub expanded: i32,
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq)]
pub struct ReachableCell {
    pub x: i32,
    pub y: i32,
    pub cost: i32,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct CellsInRangeResult {
    pub cells: Vec<ReachableCell>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct PaintCell {
    pub x: i32,
    pub y: i32,
    pub cost: i32,
    pub terrain: Option<String>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct CreateGridArgs {
    pub name: String,
    pub kind: String,
    pub orientation: String,
    pub width: i32,
    pub height: i32,
    pub default_cost: i32,
    pub connectivity: i32,
    pub mode: String,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct SetCellCostArgs {
    pub grid_id: u64,
    pub x: i32,
    pub y: i32,
    pub cost: i32,
    pub terrain: Option<String>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct PaintCellsArgs {
    pub grid_id: u64,
    pub cells: Vec<PaintCell>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct PlaceEntityArgs {
    pub grid_id: u64,
    pub x: i32,
    pub y: i32,
    pub kind: String,
    pub blocks_movement: bool,
    pub label: Option<String>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct MoveEntityArgs {
    pub entity_id: u64,
    pub to_x: i32,
    pub to_y: i32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ComputePathArgs {
    pub grid_id: u64,
    pub start_x: i32,
    pub start_y: i32,
    pub end_x: i32,
    pub end_y: i32,
    pub store_for: Option<u64>,
    pub max_expansions: Option<i32>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct CellsInRangeArgs {
    pub grid_id: u64,
    pub origin_x: i32,
    pub origin_y: i32,
    pub max_cost: i32,
}

// ---------------------------------------------------------------------------
// Math (src/math)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Coord {
    pub x: i32,
    pub y: i32,
}

const SQUARE_4: [(i32, i32); 4] = [(0, -1), (1, 0), (0, 1), (-1, 0)];
const SQUARE_8: [(i32, i32); 8] = [(0, -1), (1, -1), (1, 0), (1, 1), (0, 1), (-1, 1), (-1, 0), (-1, -1)];
const HEX_AXIAL: [(i32, i32); 6] = [(1, 0), (1, -1), (0, -1), (-1, 0), (-1, 1), (0, 1)];

/// Neighbours in the TS order. `kind` is `"square"` or `"hex"`; `connectivity` 4 or 8.
pub fn neighbors(kind: &str, c: Coord, connectivity: i32) -> Vec<Coord> {
    let offsets: &[(i32, i32)] = if kind == GRID_KIND_HEX {
        &HEX_AXIAL
    } else if connectivity == 8 {
        &SQUARE_8
    } else {
        &SQUARE_4
    };
    offsets
        .iter()
        .map(|(dx, dy)| Coord { x: c.x.wrapping_add(*dx), y: c.y.wrapping_add(*dy) })
        .collect()
}

pub fn manhattan(a: Coord, b: Coord) -> i64 {
    (a.x as i64 - b.x as i64).abs() + (a.y as i64 - b.y as i64).abs()
}

pub fn chebyshev(a: Coord, b: Coord) -> i64 {
    (a.x as i64 - b.x as i64).abs().max((a.y as i64 - b.y as i64).abs())
}

/// Hex axial distance; (q, r) stored as (x, y).
pub fn hex_distance(a: Coord, b: Coord) -> i64 {
    let (ax, ay, bx, by) = (a.x as i64, a.y as i64, b.x as i64, b.y as i64);
    ((ax - bx).abs() + (ax + ay - bx - by).abs() + (ay - by).abs()) / 2
}

pub fn distance(kind: &str, a: Coord, b: Coord, connectivity: i32) -> i64 {
    if kind == GRID_KIND_HEX {
        hex_distance(a, b)
    } else if connectivity == 8 {
        chebyshev(a, b)
    } else {
        manhattan(a, b)
    }
}

struct HeapEntry {
    key: i64,
    node: Coord,
    g_score: i64,
}

/// The TS binary min-heap, step for step, so ties break the same way.
#[derive(Default)]
struct MinHeap {
    items: Vec<HeapEntry>,
}

impl MinHeap {
    fn push(&mut self, e: HeapEntry) {
        self.items.push(e);
        let mut i = self.items.len() - 1;
        while i > 0 {
            let p = (i - 1) >> 1;
            if self.items[p].key <= self.items[i].key {
                break;
            }
            self.items.swap(p, i);
            i = p;
        }
    }

    fn pop(&mut self) -> Option<HeapEntry> {
        let n = self.items.len();
        if n == 0 {
            return None;
        }
        let last = self.items.pop().unwrap();
        if n == 1 {
            return Some(last);
        }
        let top = std::mem::replace(&mut self.items[0], last);
        let (a, n) = (&mut self.items, n - 1);
        let mut i = 0;
        loop {
            let l = i * 2 + 1;
            let r = l + 1;
            let mut s = i;
            if l < n && a[l].key < a[s].key {
                s = l;
            }
            if r < n && a[r].key < a[s].key {
                s = r;
            }
            if s == i {
                break;
            }
            a.swap(s, i);
            i = s;
        }
        Some(top)
    }
}

/// A* result: `cells` and `cost` are meaningful only when `found`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstarResult {
    pub found: bool,
    pub cells: Vec<Coord>,
    pub cost: i64,
    pub expanded: i64,
}

/// A* over an abstract graph. `cost(c) <= 0` means blocked. `heuristic` defaults to 0 and
/// `max_expansions` to 100,000.
pub fn find_path_astar(
    start: Coord,
    goal: Coord,
    cost: impl Fn(Coord) -> i64,
    neighbors: impl Fn(Coord) -> Vec<Coord>,
    heuristic: Option<&dyn Fn(Coord, Coord) -> i64>,
    max_expansions: Option<i64>,
) -> AstarResult {
    let h = |c: Coord| heuristic.map_or(0, |f| f(c, goal));
    let cap = max_expansions.unwrap_or(DEFAULT_MAX_EXPANSIONS as i64);
    if start == goal {
        return AstarResult { found: true, cells: vec![start], cost: 0, expanded: 0 };
    }
    let mut g_score: HashMap<Coord, i64> = HashMap::new();
    let mut parent: HashMap<Coord, Coord> = HashMap::new();
    let mut open = MinHeap::default();
    g_score.insert(start, 0);
    open.push(HeapEntry { key: h(start), node: start, g_score: 0 });

    let mut expanded = 0;
    while let Some(cur) = (expanded < cap).then(|| open.pop()).flatten() {
        // Skip stale heap entries (a better path landed after this one was pushed).
        if cur.g_score > g_score.get(&cur.node).copied().unwrap_or(i64::MAX) {
            continue;
        }
        expanded += 1;
        if cur.node == goal {
            let mut path = vec![cur.node];
            let mut n = cur.node;
            while let Some(p) = parent.get(&n) {
                path.push(*p);
                n = *p;
            }
            path.reverse();
            return AstarResult { found: true, cells: path, cost: cur.g_score, expanded };
        }
        for nb in neighbors(cur.node) {
            let step = cost(nb);
            if step <= 0 {
                continue;
            }
            let tentative = cur.g_score + step;
            if tentative < g_score.get(&nb).copied().unwrap_or(i64::MAX) {
                g_score.insert(nb, tentative);
                parent.insert(nb, cur.node);
                open.push(HeapEntry { key: tentative + h(nb), node: nb, g_score: tentative });
            }
        }
    }
    AstarResult { found: false, cells: Vec::new(), cost: 0, expanded }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DijkstraNode {
    pub cell: Coord,
    pub cost: i64,
    pub from: Option<Coord>,
}

/// Every cell reachable from `start` within `max_cost`, in the order TS's `Map` would hold
/// them (first discovery order).
pub fn dijkstra(
    start: Coord,
    cost: impl Fn(Coord) -> i64,
    neighbors: impl Fn(Coord) -> Vec<Coord>,
    max_cost: i64,
    max_expansions: Option<i64>,
) -> Vec<DijkstraNode> {
    let cap = max_expansions.unwrap_or(DEFAULT_MAX_EXPANSIONS as i64);
    let mut order: Vec<DijkstraNode> = Vec::new();
    let mut index: HashMap<Coord, usize> = HashMap::new();
    let mut open = MinHeap::default();
    index.insert(start, 0);
    order.push(DijkstraNode { cell: start, cost: 0, from: None });
    open.push(HeapEntry { key: 0, node: start, g_score: 0 });

    let mut expanded = 0;
    while let Some(cur) = (expanded < cap).then(|| open.pop()).flatten() {
        let Some(&k) = index.get(&cur.node) else { continue };
        if cur.g_score > order[k].cost {
            continue;
        }
        if cur.g_score > max_cost {
            break;
        }
        expanded += 1;
        for nb in neighbors(cur.node) {
            let step = cost(nb);
            if step <= 0 {
                continue;
            }
            let tentative = cur.g_score + step;
            if tentative > max_cost {
                continue;
            }
            let node = DijkstraNode { cell: nb, cost: tentative, from: Some(cur.node) };
            match index.get(&nb) {
                Some(&j) if tentative >= order[j].cost => continue,
                Some(&j) => order[j] = node, // Map.set keeps the key's position
                None => {
                    index.insert(nb, order.len());
                    order.push(node);
                }
            }
            open.push(HeapEntry { key: tentative, node: nb, g_score: tentative });
        }
    }
    order
}

// ---------------------------------------------------------------------------
// Helpers (src/procedures.ts). Each runs in the caller's transaction.
// ---------------------------------------------------------------------------

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Validates and inserts a grid. Returns its id.
pub fn create_grid(ctx: &ReducerContext, args: &CreateGridArgs, owner: &str) -> Result<u64, String> {
    let len = utf16_len(&args.name);
    if len == 0 || len > MAX_NAME_LENGTH {
        return Err(errors::INVALID_NAME.into());
    }
    if args.kind != GRID_KIND_SQUARE && args.kind != GRID_KIND_HEX {
        return Err(errors::INVALID_KIND.into());
    }
    if args.orientation != GRID_ORIENTATION_FLAT && args.orientation != GRID_ORIENTATION_POINTY {
        return Err(errors::INVALID_ORIENTATION.into());
    }
    if args.mode != GRID_MODE_OWNER && args.mode != GRID_MODE_COLLABORATIVE {
        return Err(errors::INVALID_MODE.into());
    }
    if !(1..=GRID_MAX_DIM).contains(&args.width) || !(1..=GRID_MAX_DIM).contains(&args.height) {
        return Err(errors::INVALID_DIMENSIONS.into());
    }
    if !(1..=MAX_CELL_COST).contains(&args.default_cost) {
        return Err(errors::INVALID_DEFAULT_COST.into());
    }
    if args.kind == GRID_KIND_SQUARE && args.connectivity != 4 && args.connectivity != 8 {
        return Err(errors::INVALID_CONNECTIVITY.into());
    }
    let row = ctx.db.grid().insert(Grid {
        id: 0,
        owner_user_id: owner.into(),
        name: args.name.clone(),
        kind: args.kind.clone(),
        orientation: args.orientation.clone(),
        width: args.width,
        height: args.height,
        default_cost: args.default_cost,
        connectivity: if args.kind == GRID_KIND_HEX { 6 } else { args.connectivity },
        mode: args.mode.clone(),
        created_at: ctx.timestamp,
        updated_at: ctx.timestamp,
    });
    Ok(row.id)
}

/// Only the grid's owner deletes it, in either mode. Cascades cells, entities and paths.
pub fn delete_grid(ctx: &ReducerContext, grid_id: u64, owner: &str) -> Result<(), String> {
    let grid = ctx.db.grid().id().find(grid_id).ok_or(errors::NOT_FOUND)?;
    if grid.owner_user_id != owner {
        return Err(errors::NOT_OWNER.into());
    }
    for c in ctx.db.cell_state().grid_id().filter(grid.id).collect::<Vec<_>>() {
        ctx.db.cell_state().id().delete(c.id);
    }
    for e in ctx.db.grid_entity().grid_id().filter(grid.id).collect::<Vec<_>>() {
        ctx.db.grid_entity().id().delete(e.id);
    }
    for p in ctx.db.entity_path().grid_id().filter(grid.id).collect::<Vec<_>>() {
        ctx.db.entity_path().entity_id().delete(p.entity_id);
    }
    ctx.db.grid().id().delete(grid.id);
    Ok(())
}

/// `cost <= 0` blocks; the default cost with no terrain removes the sparse row.
pub fn set_cell_cost(ctx: &ReducerContext, args: &SetCellCostArgs, owner: &str) -> Result<(), String> {
    let grid = require_grid(ctx, args.grid_id, owner)?;
    upsert_cell_state(ctx, &grid, args.x, args.y, args.cost, args.terrain.as_deref())
}

/// Up to 1,000 cells at once.
pub fn paint_cells(ctx: &ReducerContext, args: &PaintCellsArgs, owner: &str) -> Result<(), String> {
    if args.cells.len() > MAX_PAINT_CELLS {
        return Err(errors::TOO_MANY_CELLS.into());
    }
    let grid = require_grid(ctx, args.grid_id, owner)?;
    for c in &args.cells {
        upsert_cell_state(ctx, &grid, c.x, c.y, c.cost, c.terrain.as_deref())?;
    }
    Ok(())
}

/// Returns the new entity's id. The entity belongs to `owner`.
pub fn place_entity(ctx: &ReducerContext, args: &PlaceEntityArgs, owner: &str) -> Result<u64, String> {
    let len = utf16_len(&args.kind);
    if len == 0 || len > MAX_KIND_LENGTH {
        return Err(errors::INVALID_ENTITY_KIND.into());
    }
    if args.label.as_deref().map_or(0, utf16_len) > MAX_LABEL_LENGTH {
        return Err(errors::INVALID_ENTITY_LABEL.into());
    }
    let grid = require_grid(ctx, args.grid_id, owner)?;
    assert_in_bounds(&grid, Coord { x: args.x, y: args.y })?;
    let row = ctx.db.grid_entity().insert(GridEntity {
        id: 0,
        grid_id: grid.id,
        owner_user_id: owner.into(),
        x: args.x,
        y: args.y,
        kind: args.kind.clone(),
        blocks_movement: args.blocks_movement,
        label: args.label.clone(),
        created_at: ctx.timestamp,
        updated_at: ctx.timestamp,
    });
    Ok(row.id)
}

/// Moves one step to an adjacent cell that isn't blocked by terrain or by another blocking
/// entity. Only the entity's owner may move it.
pub fn move_entity(ctx: &ReducerContext, args: &MoveEntityArgs, owner: &str) -> Result<(), String> {
    let ent = ctx.db.grid_entity().id().find(args.entity_id).ok_or(errors::ENTITY_NOT_FOUND)?;
    if ent.owner_user_id != owner {
        return Err(errors::ENTITY_NOT_OWNER.into());
    }
    let grid = ctx.db.grid().id().find(ent.grid_id).ok_or(errors::NOT_FOUND)?;
    let to = Coord { x: args.to_x, y: args.to_y };
    assert_in_bounds(&grid, to)?;
    let adjacent = neighbors(&grid.kind, Coord { x: ent.x, y: ent.y }, grid.connectivity).contains(&to);
    if !adjacent {
        return Err(errors::MOVE_NOT_ADJACENT.into());
    }
    let occupied = ctx
        .db
        .grid_entity()
        .by_grid_cell()
        .filter((grid.id, to.x, to.y))
        .any(|other| other.blocks_movement && other.id != ent.id);
    if cell_cost(ctx, &grid, to) <= 0 || occupied {
        return Err(errors::MOVE_BLOCKED.into());
    }
    ctx.db.grid_entity().id().update(GridEntity {
        x: to.x,
        y: to.y,
        updated_at: ctx.timestamp,
        ..ent
    });
    Ok(())
}

/// A* over the current cost map; with `store_for`, also writes that entity's `entity_path`.
pub fn compute_path(ctx: &ReducerContext, args: &ComputePathArgs, owner: &str) -> Result<PathResult, String> {
    let grid = require_grid(ctx, args.grid_id, owner)?;
    assert_in_bounds(&grid, Coord { x: args.start_x, y: args.start_y })?;
    assert_in_bounds(&grid, Coord { x: args.end_x, y: args.end_y })?;
    let max_expansions = args.max_expansions.unwrap_or(PATH_MAX_EXPANSIONS);
    if !(1..=PATH_MAX_EXPANSIONS).contains(&max_expansions) {
        return Err(errors::INVALID_MAX_EXPANSIONS.into());
    }
    let cost_map = build_cost_map(ctx, &grid);
    let heuristic = |a: Coord, b: Coord| distance(&grid.kind, a, b, grid.connectivity);
    let result = find_path_astar(
        Coord { x: args.start_x, y: args.start_y },
        Coord { x: args.end_x, y: args.end_y },
        |c| cost_map.get(&c).copied().unwrap_or(grid.default_cost) as i64,
        |c| neighbors_in_bounds(&grid, c),
        Some(&heuristic),
        Some(max_expansions as i64),
    );
    if !result.found {
        return Ok(PathResult { found: false, cells: Vec::new(), cost: 0, expanded: result.expanded as i32 });
    }
    if result.cells.len() > MAX_RESULT_CELLS {
        return Err(errors::PATH_TOO_LONG.into());
    }
    let cells: Vec<PathCell> = result.cells.iter().map(|c| PathCell { x: c.x, y: c.y }).collect();
    if let Some(entity_id) = args.store_for {
        let entity = ctx.db.grid_entity().id().find(entity_id).ok_or(errors::ENTITY_NOT_FOUND)?;
        if entity.grid_id != grid.id {
            return Err(errors::ENTITY_GRID_MISMATCH.into());
        }
        if entity.owner_user_id != owner {
            return Err(errors::ENTITY_NOT_OWNER.into());
        }
        let row = EntityPath {
            entity_id,
            grid_id: grid.id,
            cells: cells.clone(),
            cost: result.cost as i32,
            computed_at: ctx.timestamp,
        };
        if ctx.db.entity_path().entity_id().find(entity_id).is_some() {
            ctx.db.entity_path().entity_id().update(row);
        } else {
            ctx.db.entity_path().insert(row);
        }
    }
    Ok(PathResult { found: true, cells, cost: result.cost as i32, expanded: result.expanded as i32 })
}

/// Every cell reachable from the origin within `max_cost` (Dijkstra), with its cost.
pub fn cells_in_range(ctx: &ReducerContext, args: &CellsInRangeArgs, owner: &str) -> Result<CellsInRangeResult, String> {
    let grid = require_grid(ctx, args.grid_id, owner)?;
    assert_in_bounds(&grid, Coord { x: args.origin_x, y: args.origin_y })?;
    let cost_map = build_cost_map(ctx, &grid);
    let reached = dijkstra(
        Coord { x: args.origin_x, y: args.origin_y },
        |c| cost_map.get(&c).copied().unwrap_or(grid.default_cost) as i64,
        |c| neighbors_in_bounds(&grid, c),
        args.max_cost as i64,
        Some(PATH_MAX_EXPANSIONS as i64),
    );
    if reached.len() > MAX_RESULT_CELLS {
        return Err(errors::RANGE_TOO_LARGE.into());
    }
    Ok(CellsInRangeResult {
        cells: reached
            .iter()
            .map(|n| ReachableCell { x: n.cell.x, y: n.cell.y, cost: n.cost as i32 })
            .collect(),
    })
}

/// Owner mode restricts every operation to the grid owner.
fn require_grid(ctx: &ReducerContext, grid_id: u64, owner: &str) -> Result<Grid, String> {
    let grid = ctx.db.grid().id().find(grid_id).ok_or(errors::NOT_FOUND)?;
    if grid.mode == GRID_MODE_OWNER && grid.owner_user_id != owner {
        return Err(errors::NOT_OWNER.into());
    }
    Ok(grid)
}

fn assert_in_bounds(grid: &Grid, c: Coord) -> Result<(), String> {
    if c.x < 0 || c.y < 0 || c.x >= grid.width || c.y >= grid.height {
        return Err(errors::OUT_OF_BOUNDS.into());
    }
    Ok(())
}

fn neighbors_in_bounds(grid: &Grid, c: Coord) -> Vec<Coord> {
    neighbors(&grid.kind, c, grid.connectivity)
        .into_iter()
        .filter(|n| n.x >= 0 && n.y >= 0 && n.x < grid.width && n.y < grid.height)
        .collect()
}

fn find_cell_state(ctx: &ReducerContext, grid_id: u64, c: Coord) -> Option<CellState> {
    ctx.db.cell_state().by_grid_cell().filter((grid_id, c.x, c.y)).next()
}

fn cell_cost(ctx: &ReducerContext, grid: &Grid, c: Coord) -> i32 {
    find_cell_state(ctx, grid.id, c).map_or(grid.default_cost, |s| s.cost)
}

fn upsert_cell_state(
    ctx: &ReducerContext,
    grid: &Grid,
    x: i32,
    y: i32,
    cost: i32,
    terrain: Option<&str>,
) -> Result<(), String> {
    if cost > MAX_CELL_COST {
        return Err(errors::INVALID_COST.into());
    }
    if terrain.map_or(0, utf16_len) > MAX_TERRAIN_LENGTH {
        return Err(errors::INVALID_TERRAIN.into());
    }
    assert_in_bounds(grid, Coord { x, y })?;
    let existing = find_cell_state(ctx, grid.id, Coord { x, y });
    if cost == grid.default_cost && terrain.is_none() {
        if let Some(e) = existing {
            ctx.db.cell_state().id().delete(e.id);
        }
        return Ok(());
    }
    match existing {
        Some(e) => {
            ctx.db.cell_state().id().update(CellState { cost, terrain: terrain.map(String::from), ..e });
        }
        None => {
            ctx.db.cell_state().insert(CellState {
                id: 0,
                grid_id: grid.id,
                x,
                y,
                cost,
                terrain: terrain.map(String::from),
            });
        }
    }
    Ok(())
}

/// Blocking entities override the cell cost, so painted cells can't hide them.
fn build_cost_map(ctx: &ReducerContext, grid: &Grid) -> HashMap<Coord, i32> {
    let mut map: HashMap<Coord, i32> = HashMap::new();
    for c in ctx.db.cell_state().grid_id().filter(grid.id) {
        map.insert(Coord { x: c.x, y: c.y }, c.cost);
    }
    for e in ctx.db.grid_entity().grid_id().filter(grid.id) {
        if e.blocks_movement {
            map.insert(Coord { x: e.x, y: e.y }, -1);
        }
    }
    map
}
