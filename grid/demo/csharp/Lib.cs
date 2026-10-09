// Demo for Grid.cs: the upstream README's host operations (the caller's identity as owner),
// plus procedures the test suite uses to act as any owner and to run the math helpers on
// ASCII maps (the upstream pathfinding tests). The C# twin of demo/rust.

#pragma warning disable STDB_UNSTABLE

using SpacetimeDB;

[SpacetimeDB.Type]
public partial struct DemoAstarResult
{
    public bool Found;
    public List<PathCell> Cells;
    public long Cost;
    public long Expanded;
}

public static partial class Module
{
    static Result<T, string> DemoTry<T>(Func<T> f)
    {
        try
        {
            return Result<T, string>.Ok(f());
        }
        catch (Exception e)
        {
            return Result<T, string>.Err(e.Message);
        }
    }

    static string DemoHex(Identity id)
    {
        var s = id.ToString();
        return (s.StartsWith("0x", StringComparison.OrdinalIgnoreCase) ? s[2..] : s).ToLowerInvariant();
    }

    // -- the host operations from the upstream README --------------------------------------

    [SpacetimeDB.Procedure]
    public static Result<ulong, string> CreatePlayerGrid(ProcedureContext ctx, CreateGridArgs args) =>
        DemoTry(() => ctx.WithTx(tx => GridCreate(tx.Db, tx.Timestamp, args, DemoHex(ctx.Sender))));

    [SpacetimeDB.Reducer]
    public static void MoveMyEntity(ReducerContext ctx, MoveEntityArgs args) => GridMoveEntity(ctx.Db, ctx.Timestamp, args, DemoHex(ctx.Sender));

    // -- every helper, as an explicit owner (test harness) ---------------------------------

    [SpacetimeDB.Procedure]
    public static Result<ulong, string> DemoCreateGrid(ProcedureContext ctx, string owner, CreateGridArgs args) =>
        DemoTry(() => ctx.WithTx(tx => GridCreate(tx.Db, tx.Timestamp, args, owner)));

    [SpacetimeDB.Reducer]
    public static void DemoDeleteGrid(ReducerContext ctx, string owner, ulong gridId) => GridDelete(ctx.Db, gridId, owner);

    [SpacetimeDB.Reducer]
    public static void DemoSetCellCost(ReducerContext ctx, string owner, SetCellCostArgs args) => GridSetCellCost(ctx.Db, args, owner);

    [SpacetimeDB.Reducer]
    public static void DemoPaintCells(ReducerContext ctx, string owner, PaintCellsArgs args) => GridPaintCells(ctx.Db, args, owner);

    [SpacetimeDB.Procedure]
    public static Result<ulong, string> DemoPlaceEntity(ProcedureContext ctx, string owner, PlaceEntityArgs args) =>
        DemoTry(() => ctx.WithTx(tx => GridPlaceEntity(tx.Db, tx.Timestamp, args, owner)));

    [SpacetimeDB.Reducer]
    public static void DemoMoveEntity(ReducerContext ctx, string owner, MoveEntityArgs args) => GridMoveEntity(ctx.Db, ctx.Timestamp, args, owner);

    [SpacetimeDB.Procedure]
    public static Result<PathResult, string> DemoComputePath(ProcedureContext ctx, string owner, ComputePathArgs args) =>
        DemoTry(() => ctx.WithTx(tx => GridComputePath(tx.Db, tx.Timestamp, args, owner)));

    [SpacetimeDB.Procedure]
    public static Result<CellsInRangeResult, string> DemoCellsInRange(ProcedureContext ctx, string owner, CellsInRangeArgs args) =>
        DemoTry(() => ctx.WithTx(tx => GridCellsInRange(tx.Db, args, owner)));

    // -- the math helpers on ASCII maps: '.' open (1), '#' wall, '2'..'9' cost -------------

    static Func<Coord, long> DemoMapCost(List<string> rows) => c =>
    {
        if (c.Y < 0 || c.Y >= rows.Count || c.X < 0 || c.X >= rows[c.Y].Length) return 1;
        var ch = rows[c.Y][c.X];
        return ch == '#' ? -1 : ch is >= '2' and <= '9' ? ch - '0' : 1;
    };

    /// <summary>A* on an ASCII map. bounded = false ignores the map's edges.</summary>
    [SpacetimeDB.Procedure]
    public static DemoAstarResult DemoAstar(ProcedureContext ctx, List<string> rows, PathCell start, PathCell goal, string kind, int connectivity, bool useHeuristic, bool bounded)
    {
        int w = rows.Count > 0 ? rows[0].Length : 0, h = rows.Count;
        var r = GridMath.FindPathAstar(
            new Coord(start.X, start.Y),
            new Coord(goal.X, goal.Y),
            DemoMapCost(rows),
            c => GridMath.Neighbors(kind, c, connectivity).Where(n => !bounded || (n.X >= 0 && n.Y >= 0 && n.X < w && n.Y < h)).ToList(),
            useHeuristic ? (a, b) => GridMath.Distance(kind, a, b, connectivity) : null);
        return new DemoAstarResult { Found = r.Found, Cells = r.Cells.Select(c => new PathCell { X = c.X, Y = c.Y }).ToList(), Cost = r.Cost, Expanded = r.Expanded };
    }

    [SpacetimeDB.Procedure]
    public static List<ReachableCell> DemoDijkstra(ProcedureContext ctx, List<string> rows, PathCell start, long maxCost, string kind, int connectivity)
    {
        int w = rows.Count > 0 ? rows[0].Length : 0, h = rows.Count;
        return GridMath.Dijkstra(
                new Coord(start.X, start.Y),
                DemoMapCost(rows),
                c => GridMath.Neighbors(kind, c, connectivity).Where(n => n.X >= 0 && n.Y >= 0 && n.X < w && n.Y < h).ToList(),
                maxCost)
            .Select(n => new ReachableCell { X = n.Cell.X, Y = n.Cell.Y, Cost = (int)n.Cost })
            .ToList();
    }
}
