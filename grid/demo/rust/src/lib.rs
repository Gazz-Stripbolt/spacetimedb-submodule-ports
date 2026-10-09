//! Demo for grid.rs: the upstream README's host operations (the caller's identity as owner),
//! plus procedures the test suite uses to act as any owner and to run the math helpers on
//! ASCII maps (the upstream pathfinding tests).

#[path = "../../../rust/grid.rs"]
pub mod grid;

use grid::*;
use spacetimedb::{ProcedureContext, ReducerContext, SpacetimeType};

// -- the host operations from the upstream README -------------------------------------------

#[spacetimedb::procedure]
pub fn create_player_grid(ctx: &mut ProcedureContext, args: CreateGridArgs) -> Result<u64, String> {
    let owner = ctx.sender().to_hex().to_string();
    ctx.try_with_tx(|tx| grid::create_grid(tx, &args, &owner))
}

#[spacetimedb::reducer]
pub fn move_my_entity(ctx: &ReducerContext, args: MoveEntityArgs) -> Result<(), String> {
    grid::move_entity(ctx, &args, &ctx.sender().to_hex())
}

// -- every helper, as an explicit owner (test harness) ---------------------------------------

#[spacetimedb::procedure]
pub fn demo_create_grid(ctx: &mut ProcedureContext, owner: String, args: CreateGridArgs) -> Result<u64, String> {
    ctx.try_with_tx(|tx| grid::create_grid(tx, &args, &owner))
}

#[spacetimedb::reducer]
pub fn demo_delete_grid(ctx: &ReducerContext, owner: String, grid_id: u64) -> Result<(), String> {
    grid::delete_grid(ctx, grid_id, &owner)
}

#[spacetimedb::reducer]
pub fn demo_set_cell_cost(ctx: &ReducerContext, owner: String, args: SetCellCostArgs) -> Result<(), String> {
    grid::set_cell_cost(ctx, &args, &owner)
}

#[spacetimedb::reducer]
pub fn demo_paint_cells(ctx: &ReducerContext, owner: String, args: PaintCellsArgs) -> Result<(), String> {
    grid::paint_cells(ctx, &args, &owner)
}

#[spacetimedb::procedure]
pub fn demo_place_entity(ctx: &mut ProcedureContext, owner: String, args: PlaceEntityArgs) -> Result<u64, String> {
    ctx.try_with_tx(|tx| grid::place_entity(tx, &args, &owner))
}

#[spacetimedb::reducer]
pub fn demo_move_entity(ctx: &ReducerContext, owner: String, args: MoveEntityArgs) -> Result<(), String> {
    grid::move_entity(ctx, &args, &owner)
}

#[spacetimedb::procedure]
pub fn demo_compute_path(ctx: &mut ProcedureContext, owner: String, args: ComputePathArgs) -> Result<PathResult, String> {
    ctx.try_with_tx(|tx| grid::compute_path(tx, &args, &owner))
}

#[spacetimedb::procedure]
pub fn demo_cells_in_range(ctx: &mut ProcedureContext, owner: String, args: CellsInRangeArgs) -> Result<CellsInRangeResult, String> {
    ctx.try_with_tx(|tx| grid::cells_in_range(tx, &args, &owner))
}

// -- the math helpers on ASCII maps: '.' open (1), '#' wall, '2'..'9' cost ----------------

#[derive(SpacetimeType)]
pub struct DemoAstar {
    pub found: bool,
    pub cells: Vec<PathCell>,
    pub cost: i64,
    pub expanded: i64,
}

fn map_cost(rows: &[String]) -> impl Fn(Coord) -> i64 + '_ {
    move |c: Coord| match rows.get(c.y as usize).and_then(|r| r.as_bytes().get(c.x as usize)) {
        Some(b'#') => -1,
        Some(d @ b'2'..=b'9') => (d - b'0') as i64,
        _ => 1,
    }
}

/// A* on an ASCII map. `bounded = false` ignores the map's edges (every cell costs 1 outside it).
#[spacetimedb::procedure]
#[allow(clippy::too_many_arguments)]
pub fn demo_astar(
    _ctx: &mut ProcedureContext,
    rows: Vec<String>,
    start: PathCell,
    goal: PathCell,
    kind: String,
    connectivity: i32,
    use_heuristic: bool,
    bounded: bool,
) -> DemoAstar {
    let (w, h) = (rows.first().map_or(0, |r| r.len()) as i32, rows.len() as i32);
    let heuristic = |a: Coord, b: Coord| distance(&kind, a, b, connectivity);
    let r = find_path_astar(
        Coord { x: start.x, y: start.y },
        Coord { x: goal.x, y: goal.y },
        map_cost(&rows),
        |c| {
            neighbors(&kind, c, connectivity)
                .into_iter()
                .filter(|n| !bounded || (n.x >= 0 && n.y >= 0 && n.x < w && n.y < h))
                .collect()
        },
        use_heuristic.then_some(&heuristic as &dyn Fn(Coord, Coord) -> i64),
        None,
    );
    DemoAstar {
        found: r.found,
        cells: r.cells.iter().map(|c| PathCell { x: c.x, y: c.y }).collect(),
        cost: r.cost,
        expanded: r.expanded,
    }
}

#[spacetimedb::procedure]
pub fn demo_dijkstra(
    _ctx: &mut ProcedureContext,
    rows: Vec<String>,
    start: PathCell,
    max_cost: i64,
    kind: String,
    connectivity: i32,
) -> Vec<ReachableCell> {
    let (w, h) = (rows.first().map_or(0, |r| r.len()) as i32, rows.len() as i32);
    dijkstra(
        Coord { x: start.x, y: start.y },
        map_cost(&rows),
        |c| {
            neighbors(&kind, c, connectivity)
                .into_iter()
                .filter(|n| n.x >= 0 && n.y >= 0 && n.x < w && n.y < h)
                .collect()
        },
        max_cost,
        None,
    )
    .iter()
    .map(|n| ReachableCell { x: n.cell.x, y: n.cell.y, cost: n.cost as i32 })
    .collect()
}
