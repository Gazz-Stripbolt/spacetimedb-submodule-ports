# grid: square and hex maps, units, obstacles, pathfinding and movement ranges

Port of [`@spacetimedb/grid`](https://github.com/clockworklabs/SpacetimeDB/pull/5908) at
`f6ed7dcd1c8f853c97fc7e9f126606fca7adadac`.

**Schema:** private tables `grid`, `cell_state`, `grid_entity` and `entity_path`, with the same columns and indexes
as TS (including `by_grid_cell` on `(grid_id, x, y)`). Like the TS package, it defines **no reducers**: your module
exposes its own operations and passes the owner string it trusts (an identity, a user id, an actor id).

**Identical results:** A* and Dijkstra are ported step for step (the same binary heap, neighbour order and
tie-breaking), so paths, costs, expansion counts and the order of reachable cells match TypeScript exactly. The tests
check 240 random maps against answers generated from the real TS math
([`tools/gen-grid-vectors.mjs`](../tools/gen-grid-vectors.mjs)).

```rust
#[spacetimedb::procedure]
pub fn create_player_grid(ctx: &mut ProcedureContext, args: grid::CreateGridArgs) -> Result<u64, String> {
    let owner = ctx.sender().to_hex().to_string();
    ctx.try_with_tx(|tx| grid::create_grid(tx, &args, &owner))
}

#[spacetimedb::reducer]
pub fn move_my_entity(ctx: &ReducerContext, args: grid::MoveEntityArgs) -> Result<(), String> {
    grid::move_entity(ctx, &args, &ctx.sender().to_hex())
}
```

```csharp
[Procedure]
public static Result<ulong, string> CreatePlayerGrid(ProcedureContext ctx, CreateGridArgs args) =>
    ... ctx.WithTx(tx => GridCreate(tx.Db, tx.Timestamp, args, ownerOf(ctx.Sender))) ...;

[Reducer]
public static void MoveMyEntity(ReducerContext ctx, MoveEntityArgs args) =>
    GridMoveEntity(ctx.Db, ctx.Timestamp, args, ownerOf(ctx.Sender));
```

| TypeScript | Rust | C# |
|---|---|---|
| `createGrid(tx, args, owner)` → id | `create_grid(ctx, &CreateGridArgs, owner)` | `GridCreate(db, now, CreateGridArgs, owner)` |
| `deleteGrid` / `setCellCost` / `paintCells` | `delete_grid` / `set_cell_cost` / `paint_cells` | `GridDelete` / `GridSetCellCost` / `GridPaintCells` |
| `placeEntity` / `moveEntity` | `place_entity` / `move_entity` | `GridPlaceEntity` / `GridMoveEntity` |
| `computePath` / `cellsInRange` | `compute_path` / `cells_in_range` | `GridComputePath` / `GridCellsInRange` |
| `*Params` objects | `CreateGridArgs`, `SetCellCostArgs`, `PaintCellsArgs`, `PlaceEntityArgs`, `MoveEntityArgs`, `ComputePathArgs`, `CellsInRangeArgs` | the same names |
| `neighbors`, `distance`, `findPathAstar`, `dijkstra` (`math/`) | `grid::neighbors`, `distance`, `find_path_astar`, `dijkstra` | `GridMath.Neighbors`, `Distance`, `FindPathAstar`, `Dijkstra` |

**Differences:**
- **Arguments:** the `*Params` field lists are argument structs (`SpacetimeType` / `[SpacetimeDB.Type]`), so a host
  can take one as a single reducer argument or declare the fields itself.
- **Errors:** `Err(String)` / thrown `Exception`s carrying the same `grid.*` codes.

**Tests:** in [`tests/grid.test.mjs`](../tests/grid.test.mjs):
- the upstream pathfinding and procedure tests
- the 240 generated vectors
- every helper end to end: validation, owner and collaborative modes, sparse cells, adjacency on square 4/8 and hex
  grids, stored paths, range limits, and delete cascades
