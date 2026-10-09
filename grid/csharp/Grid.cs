// Grid.cs: square and hex maps, units, obstacles, pathfinding and movement ranges, for C# modules.
//
// Unofficial C# port of @spacetimedb/grid (SpacetimeDB PR #5908, commit
// f6ed7dcd1c8f853c97fc7e9f126606fca7adadac, package spacetime-grid-ts). Licensed Apache-2.0,
// like the original. Original design and behaviour by Clockwork Labs; this port is by Tinker
// (Gazz-Stripbolt).
//
// Same private tables (grid, cell_state, grid_entity, entity_path), limits, error codes and
// algorithms as the TS package. A* and Dijkstra are ported step for step (same binary heap,
// same neighbour order, same tie-breaking), so they return the same paths, costs, expansion
// counts and cell order as TypeScript.
//
// Like the TS package, it defines no reducers: your module exposes its own operations and
// passes the owner it trusts (an identity, a user id, any string):
//
//     [Procedure]
//     public static Result<ulong, string> CreatePlayerGrid(ProcedureContext ctx, CreateGridArgs args)
//     {
//         var owner = ctx.Sender.ToString();
//         return ... ctx.WithTx(tx => GridCreate(tx.Db, tx.Timestamp, args, owner)) ...;
//     }
//
// Changes from the TypeScript original:
// - The *Params objects become [SpacetimeDB.Type] argument structs (CreateGridArgs, ...).
// - Helpers take the transaction's Db and Timestamp, so they work from reducers and procedures,
//   and throw Exceptions carrying the same grid.* codes.

#pragma warning disable STDB_UNSTABLE
#nullable enable

using SpacetimeDB;

[SpacetimeDB.Type]
public partial struct PathCell
{
    public int X;
    public int Y;
}

[SpacetimeDB.Type]
public partial struct PathResult
{
    public bool Found;
    public List<PathCell> Cells;
    public int Cost;
    public int Expanded;
}

[SpacetimeDB.Type]
public partial struct ReachableCell
{
    public int X;
    public int Y;
    public int Cost;
}

[SpacetimeDB.Type]
public partial struct CellsInRangeResult
{
    public List<ReachableCell> Cells;
}

[SpacetimeDB.Type]
public partial struct PaintCell
{
    public int X;
    public int Y;
    public int Cost;
    public string? Terrain;
}

[SpacetimeDB.Type]
public partial struct CreateGridArgs
{
    public string Name;
    public string Kind;
    public string Orientation;
    public int Width;
    public int Height;
    public int DefaultCost;
    public int Connectivity;
    public string Mode;
}

[SpacetimeDB.Type]
public partial struct SetCellCostArgs
{
    public ulong GridId;
    public int X;
    public int Y;
    public int Cost;
    public string? Terrain;
}

[SpacetimeDB.Type]
public partial struct PaintCellsArgs
{
    public ulong GridId;
    public List<PaintCell> Cells;
}

[SpacetimeDB.Type]
public partial struct PlaceEntityArgs
{
    public ulong GridId;
    public int X;
    public int Y;
    public string Kind;
    public bool BlocksMovement;
    public string? Label;
}

[SpacetimeDB.Type]
public partial struct MoveEntityArgs
{
    public ulong EntityId;
    public int ToX;
    public int ToY;
}

[SpacetimeDB.Type]
public partial struct ComputePathArgs
{
    public ulong GridId;
    public int StartX;
    public int StartY;
    public int EndX;
    public int EndY;
    public ulong? StoreFor;
    public int? MaxExpansions;
}

[SpacetimeDB.Type]
public partial struct CellsInRangeArgs
{
    public ulong GridId;
    public int OriginX;
    public int OriginY;
    public int MaxCost;
}

/// <summary>A grid coordinate (hex grids use axial (q, r) as (X, Y)).</summary>
public readonly record struct Coord(int X, int Y);

/// <summary>The grid math (src/math): neighbours, distances, A* and Dijkstra.</summary>
public static class GridMath
{
    public const string KindSquare = "square";
    public const string KindHex = "hex";
    const long DefaultMaxExpansions = 100_000;

    static readonly (int, int)[] Square4 = [(0, -1), (1, 0), (0, 1), (-1, 0)];
    static readonly (int, int)[] Square8 = [(0, -1), (1, -1), (1, 0), (1, 1), (0, 1), (-1, 1), (-1, 0), (-1, -1)];
    static readonly (int, int)[] HexAxial = [(1, 0), (1, -1), (0, -1), (-1, 0), (-1, 1), (0, 1)];

    /// <summary>Neighbours in the TS order.</summary>
    public static List<Coord> Neighbors(string kind, Coord c, int connectivity = 4)
    {
        var offsets = kind == KindHex ? HexAxial : connectivity == 8 ? Square8 : Square4;
        return offsets.Select(o => new Coord(unchecked(c.X + o.Item1), unchecked(c.Y + o.Item2))).ToList();
    }

    public static long Manhattan(Coord a, Coord b) => Math.Abs((long)a.X - b.X) + Math.Abs((long)a.Y - b.Y);

    public static long Chebyshev(Coord a, Coord b) => Math.Max(Math.Abs((long)a.X - b.X), Math.Abs((long)a.Y - b.Y));

    /// <summary>Hex axial distance; (q, r) stored as (x, y).</summary>
    public static long HexDistance(Coord a, Coord b) =>
        (Math.Abs((long)a.X - b.X) + Math.Abs((long)a.X + a.Y - b.X - b.Y) + Math.Abs((long)a.Y - b.Y)) / 2;

    public static long Distance(string kind, Coord a, Coord b, int connectivity = 4) =>
        kind == KindHex ? HexDistance(a, b) : connectivity == 8 ? Chebyshev(a, b) : Manhattan(a, b);

    readonly record struct HeapEntry(long Key, Coord Node, long GScore);

    /// <summary>The TS binary min-heap, step for step, so ties break the same way.</summary>
    sealed class MinHeap
    {
        readonly List<HeapEntry> a = [];
        public int Count => a.Count;

        public void Push(HeapEntry e)
        {
            a.Add(e);
            var i = a.Count - 1;
            while (i > 0)
            {
                var p = (i - 1) >> 1;
                if (a[p].Key <= a[i].Key) break;
                (a[p], a[i]) = (a[i], a[p]);
                i = p;
            }
        }

        public HeapEntry Pop()
        {
            var n = a.Count;
            var top = a[0];
            var last = a[n - 1];
            a.RemoveAt(n - 1);
            if (n > 1)
            {
                a[0] = last;
                var i = 0;
                n--;
                while (true)
                {
                    var l = i * 2 + 1;
                    var r = l + 1;
                    var s = i;
                    if (l < n && a[l].Key < a[s].Key) s = l;
                    if (r < n && a[r].Key < a[s].Key) s = r;
                    if (s == i) break;
                    (a[s], a[i]) = (a[i], a[s]);
                    i = s;
                }
            }
            return top;
        }
    }

    /// <summary>A* result: Cells and Cost are meaningful only when Found.</summary>
    public readonly record struct AstarResult(bool Found, List<Coord> Cells, long Cost, long Expanded);

    /// <summary>A* over an abstract graph. cost(c) &lt;= 0 means blocked. heuristic defaults to 0, maxExpansions to 100,000.</summary>
    public static AstarResult FindPathAstar(Coord start, Coord goal, Func<Coord, long> cost, Func<Coord, List<Coord>> neighbors, Func<Coord, Coord, long>? heuristic = null, long? maxExpansions = null)
    {
        long H(Coord c) => heuristic?.Invoke(c, goal) ?? 0;
        var cap = maxExpansions ?? DefaultMaxExpansions;
        if (start == goal) return new(true, [start], 0, 0);
        var gScore = new Dictionary<Coord, long> { [start] = 0 };
        var parent = new Dictionary<Coord, Coord>();
        var open = new MinHeap();
        open.Push(new(H(start), start, 0));
        long expanded = 0;
        while (open.Count > 0)
        {
            if (expanded >= cap) break;
            var cur = open.Pop();
            // Skip stale heap entries (a better path landed after this one was pushed).
            if (cur.GScore > (gScore.TryGetValue(cur.Node, out var g) ? g : long.MaxValue)) continue;
            expanded++;
            if (cur.Node == goal)
            {
                var path = new List<Coord> { cur.Node };
                var n = cur.Node;
                while (parent.TryGetValue(n, out var p))
                {
                    path.Add(p);
                    n = p;
                }
                path.Reverse();
                return new(true, path, cur.GScore, expanded);
            }
            foreach (var nb in neighbors(cur.Node))
            {
                var step = cost(nb);
                if (step <= 0) continue;
                var tentative = cur.GScore + step;
                if (tentative < (gScore.TryGetValue(nb, out var old) ? old : long.MaxValue))
                {
                    gScore[nb] = tentative;
                    parent[nb] = cur.Node;
                    open.Push(new(tentative + H(nb), nb, tentative));
                }
            }
        }
        return new(false, [], 0, expanded);
    }

    public readonly record struct DijkstraNode(Coord Cell, long Cost, Coord? From);

    /// <summary>Every cell reachable from start within maxCost, in the order TS's Map would hold them.</summary>
    public static List<DijkstraNode> Dijkstra(Coord start, Func<Coord, long> cost, Func<Coord, List<Coord>> neighbors, long maxCost, long? maxExpansions = null)
    {
        var cap = maxExpansions ?? DefaultMaxExpansions;
        var order = new List<DijkstraNode> { new(start, 0, null) };
        var index = new Dictionary<Coord, int> { [start] = 0 };
        var open = new MinHeap();
        open.Push(new(0, start, 0));
        long expanded = 0;
        while (open.Count > 0)
        {
            if (expanded >= cap) break;
            var cur = open.Pop();
            if (!index.TryGetValue(cur.Node, out var k) || cur.GScore > order[k].Cost) continue;
            if (cur.GScore > maxCost) break;
            expanded++;
            foreach (var nb in neighbors(cur.Node))
            {
                var step = cost(nb);
                if (step <= 0) continue;
                var tentative = cur.GScore + step;
                if (tentative > maxCost) continue;
                var node = new DijkstraNode(nb, tentative, cur.Node);
                if (index.TryGetValue(nb, out var j))
                {
                    if (tentative >= order[j].Cost) continue;
                    order[j] = node; // Map.set keeps the key's position
                }
                else
                {
                    index[nb] = order.Count;
                    order.Add(node);
                }
                open.Push(new(tentative, nb, tentative));
            }
        }
        return order;
    }
}

public static partial class Module
{
    public const string GridKindSquare = "square";
    public const string GridKindHex = "hex";
    public const string GridOrientationFlat = "flat";
    public const string GridOrientationPointy = "pointy";
    public const string GridModeOwner = "owner";
    public const string GridModeCollaborative = "collaborative";

    const int GridMaxDim = 1024;
    /// <summary>Path costs stay inside i32: at most PathMaxExpansions steps of MaxCellCost.</summary>
    const int GridMaxCellCost = 10_000;
    const int GridPathMaxExpansions = 50_000;
    const int GridMaxResultCells = 5_000;
    const int GridMaxPaintCells = 1_000;
    const int GridMaxNameLength = 128;
    const int GridMaxKindLength = 64;
    const int GridMaxLabelLength = 256;
    const int GridMaxTerrainLength = 64;

    /// <summary>The grid.* codes.</summary>
    public static class GridErrors
    {
        public const string InvalidName = "grid.invalid_name";
        public const string InvalidKind = "grid.invalid_kind";
        public const string InvalidOrientation = "grid.invalid_orientation";
        public const string InvalidMode = "grid.invalid_mode";
        public const string InvalidDimensions = "grid.invalid_dimensions";
        public const string InvalidDefaultCost = "grid.invalid_default_cost";
        public const string InvalidConnectivity = "grid.invalid_connectivity";
        public const string InvalidCost = "grid.invalid_cost";
        public const string InvalidTerrain = "grid.invalid_terrain";
        public const string TooManyCells = "grid.too_many_cells";
        public const string InvalidEntityKind = "grid.invalid_entity_kind";
        public const string InvalidEntityLabel = "grid.invalid_entity_label";
        public const string InvalidMaxExpansions = "grid.invalid_max_expansions";
        public const string NotFound = "grid.not_found";
        public const string NotOwner = "grid.not_owner";
        public const string EntityNotFound = "grid.entity_not_found";
        public const string EntityNotOwner = "grid.entity_not_owner";
        public const string EntityGridMismatch = "grid.entity_grid_mismatch";
        public const string OutOfBounds = "grid.out_of_bounds";
        public const string MoveNotAdjacent = "grid.move_not_adjacent";
        public const string MoveBlocked = "grid.move_blocked";
        public const string PathTooLong = "grid.path_too_long";
        public const string RangeTooLarge = "grid.range_too_large";
    }

    /// <summary>OwnerUserId is opaque: an identity, an application user id, or a host-defined actor id.</summary>
    [SpacetimeDB.Table(Accessor = "Grid")]
    public partial struct Grid
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        [SpacetimeDB.Index.BTree]
        public string OwnerUserId;
        public string Name;
        public string Kind;
        public string Orientation;
        public int Width;
        public int Height;
        public int DefaultCost;
        public int Connectivity;
        public string Mode;
        public Timestamp CreatedAt;
        public Timestamp UpdatedAt;
    }

    /// <summary>Sparse: only non-default cells have a row. Cost &lt;= 0 means blocked.</summary>
    [SpacetimeDB.Table(Accessor = "CellState")]
    [SpacetimeDB.Index.BTree(Accessor = "ByGridCell", Columns = new[] { nameof(GridId), nameof(X), nameof(Y) })]
    public partial struct CellState
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        [SpacetimeDB.Index.BTree]
        public ulong GridId;
        public int X;
        public int Y;
        public int Cost;
        public string? Terrain;
    }

    [SpacetimeDB.Table(Accessor = "GridEntity")]
    [SpacetimeDB.Index.BTree(Accessor = "ByGridCell", Columns = new[] { nameof(GridId), nameof(X), nameof(Y) })]
    public partial struct GridEntity
    {
        [SpacetimeDB.PrimaryKey, SpacetimeDB.AutoInc]
        public ulong Id;
        [SpacetimeDB.Index.BTree]
        public ulong GridId;
        [SpacetimeDB.Index.BTree]
        public string OwnerUserId;
        public int X;
        public int Y;
        public string Kind;
        public bool BlocksMovement;
        public string? Label;
        public Timestamp CreatedAt;
        public Timestamp UpdatedAt;
    }

    /// <summary>Not auto-invalidated; call GridComputePath again for a fresh route.</summary>
    [SpacetimeDB.Table(Accessor = "EntityPath")]
    public partial struct EntityPath
    {
        [SpacetimeDB.PrimaryKey]
        public ulong EntityId;
        [SpacetimeDB.Index.BTree]
        public ulong GridId;
        public List<PathCell> Cells;
        public int Cost;
        public Timestamp ComputedAt;
    }

    /// <summary>Validates and inserts a grid. Returns its id.</summary>
    public static ulong GridCreate(Local db, Timestamp now, CreateGridArgs args, string owner)
    {
        if (args.Name.Length == 0 || args.Name.Length > GridMaxNameLength) throw new Exception(GridErrors.InvalidName);
        if (args.Kind is not (GridKindSquare or GridKindHex)) throw new Exception(GridErrors.InvalidKind);
        if (args.Orientation is not (GridOrientationFlat or GridOrientationPointy)) throw new Exception(GridErrors.InvalidOrientation);
        if (args.Mode is not (GridModeOwner or GridModeCollaborative)) throw new Exception(GridErrors.InvalidMode);
        if (args.Width < 1 || args.Width > GridMaxDim || args.Height < 1 || args.Height > GridMaxDim) throw new Exception(GridErrors.InvalidDimensions);
        if (args.DefaultCost < 1 || args.DefaultCost > GridMaxCellCost) throw new Exception(GridErrors.InvalidDefaultCost);
        if (args.Kind == GridKindSquare && args.Connectivity is not (4 or 8)) throw new Exception(GridErrors.InvalidConnectivity);
        return db.Grid.Insert(new Grid
        {
            Id = 0,
            OwnerUserId = owner,
            Name = args.Name,
            Kind = args.Kind,
            Orientation = args.Orientation,
            Width = args.Width,
            Height = args.Height,
            DefaultCost = args.DefaultCost,
            Connectivity = args.Kind == GridKindHex ? 6 : args.Connectivity,
            Mode = args.Mode,
            CreatedAt = now,
            UpdatedAt = now,
        }).Id;
    }

    /// <summary>Only the grid's owner deletes it, in either mode. Cascades cells, entities and paths.</summary>
    public static void GridDelete(Local db, ulong gridId, string owner)
    {
        var grid = db.Grid.Id.Find(gridId) ?? throw new Exception(GridErrors.NotFound);
        if (grid.OwnerUserId != owner) throw new Exception(GridErrors.NotOwner);
        foreach (var c in db.CellState.GridId.Filter(grid.Id).ToList()) db.CellState.Id.Delete(c.Id);
        foreach (var e in db.GridEntity.GridId.Filter(grid.Id).ToList()) db.GridEntity.Id.Delete(e.Id);
        foreach (var p in db.EntityPath.GridId.Filter(grid.Id).ToList()) db.EntityPath.EntityId.Delete(p.EntityId);
        db.Grid.Id.Delete(grid.Id);
    }

    /// <summary>Cost &lt;= 0 blocks; the default cost with no terrain removes the sparse row.</summary>
    public static void GridSetCellCost(Local db, SetCellCostArgs args, string owner)
    {
        var grid = GridRequire(db, args.GridId, owner);
        GridUpsertCellState(db, grid, args.X, args.Y, args.Cost, args.Terrain);
    }

    /// <summary>Up to 1,000 cells at once.</summary>
    public static void GridPaintCells(Local db, PaintCellsArgs args, string owner)
    {
        if (args.Cells.Count > GridMaxPaintCells) throw new Exception(GridErrors.TooManyCells);
        var grid = GridRequire(db, args.GridId, owner);
        foreach (var c in args.Cells) GridUpsertCellState(db, grid, c.X, c.Y, c.Cost, c.Terrain);
    }

    /// <summary>Returns the new entity's id. The entity belongs to owner.</summary>
    public static ulong GridPlaceEntity(Local db, Timestamp now, PlaceEntityArgs args, string owner)
    {
        if (args.Kind.Length == 0 || args.Kind.Length > GridMaxKindLength) throw new Exception(GridErrors.InvalidEntityKind);
        if ((args.Label?.Length ?? 0) > GridMaxLabelLength) throw new Exception(GridErrors.InvalidEntityLabel);
        var grid = GridRequire(db, args.GridId, owner);
        GridAssertInBounds(grid, new Coord(args.X, args.Y));
        return db.GridEntity.Insert(new GridEntity
        {
            Id = 0,
            GridId = grid.Id,
            OwnerUserId = owner,
            X = args.X,
            Y = args.Y,
            Kind = args.Kind,
            BlocksMovement = args.BlocksMovement,
            Label = args.Label,
            CreatedAt = now,
            UpdatedAt = now,
        }).Id;
    }

    /// <summary>Moves one step to an adjacent cell that isn't blocked by terrain or another blocking entity. Only the entity's owner may move it.</summary>
    public static void GridMoveEntity(Local db, Timestamp now, MoveEntityArgs args, string owner)
    {
        var ent = db.GridEntity.Id.Find(args.EntityId) ?? throw new Exception(GridErrors.EntityNotFound);
        if (ent.OwnerUserId != owner) throw new Exception(GridErrors.EntityNotOwner);
        var grid = db.Grid.Id.Find(ent.GridId) ?? throw new Exception(GridErrors.NotFound);
        var to = new Coord(args.ToX, args.ToY);
        GridAssertInBounds(grid, to);
        if (!GridMath.Neighbors(grid.Kind, new Coord(ent.X, ent.Y), grid.Connectivity).Contains(to)) throw new Exception(GridErrors.MoveNotAdjacent);
        var occupied = db.GridEntity.ByGridCell.Filter((grid.Id, to.X, to.Y)).Any(o => o.BlocksMovement && o.Id != ent.Id);
        if (GridCellCost(db, grid, to) <= 0 || occupied) throw new Exception(GridErrors.MoveBlocked);
        db.GridEntity.Id.Update(ent with { X = to.X, Y = to.Y, UpdatedAt = now });
    }

    /// <summary>A* over the current cost map; with StoreFor, also writes that entity's entity_path.</summary>
    public static PathResult GridComputePath(Local db, Timestamp now, ComputePathArgs args, string owner)
    {
        var grid = GridRequire(db, args.GridId, owner);
        GridAssertInBounds(grid, new Coord(args.StartX, args.StartY));
        GridAssertInBounds(grid, new Coord(args.EndX, args.EndY));
        var maxExpansions = args.MaxExpansions ?? GridPathMaxExpansions;
        if (maxExpansions < 1 || maxExpansions > GridPathMaxExpansions) throw new Exception(GridErrors.InvalidMaxExpansions);
        var costMap = GridBuildCostMap(db, grid);
        var result = GridMath.FindPathAstar(
            new Coord(args.StartX, args.StartY),
            new Coord(args.EndX, args.EndY),
            c => costMap.TryGetValue(c, out var v) ? v : grid.DefaultCost,
            c => GridNeighborsInBounds(grid, c),
            (a, b) => GridMath.Distance(grid.Kind, a, b, grid.Connectivity),
            maxExpansions);
        if (!result.Found) return new PathResult { Found = false, Cells = [], Cost = 0, Expanded = (int)result.Expanded };
        if (result.Cells.Count > GridMaxResultCells) throw new Exception(GridErrors.PathTooLong);
        var cells = result.Cells.Select(c => new PathCell { X = c.X, Y = c.Y }).ToList();
        if (args.StoreFor is { } entityId)
        {
            var entity = db.GridEntity.Id.Find(entityId) ?? throw new Exception(GridErrors.EntityNotFound);
            if (entity.GridId != grid.Id) throw new Exception(GridErrors.EntityGridMismatch);
            if (entity.OwnerUserId != owner) throw new Exception(GridErrors.EntityNotOwner);
            var row = new EntityPath { EntityId = entityId, GridId = grid.Id, Cells = cells, Cost = (int)result.Cost, ComputedAt = now };
            if (db.EntityPath.EntityId.Find(entityId) is not null) db.EntityPath.EntityId.Update(row);
            else db.EntityPath.Insert(row);
        }
        return new PathResult { Found = true, Cells = cells, Cost = (int)result.Cost, Expanded = (int)result.Expanded };
    }

    /// <summary>Every cell reachable from the origin within MaxCost (Dijkstra), with its cost.</summary>
    public static CellsInRangeResult GridCellsInRange(Local db, CellsInRangeArgs args, string owner)
    {
        var grid = GridRequire(db, args.GridId, owner);
        GridAssertInBounds(grid, new Coord(args.OriginX, args.OriginY));
        var costMap = GridBuildCostMap(db, grid);
        var reached = GridMath.Dijkstra(
            new Coord(args.OriginX, args.OriginY),
            c => costMap.TryGetValue(c, out var v) ? v : grid.DefaultCost,
            c => GridNeighborsInBounds(grid, c),
            args.MaxCost,
            GridPathMaxExpansions);
        if (reached.Count > GridMaxResultCells) throw new Exception(GridErrors.RangeTooLarge);
        return new CellsInRangeResult { Cells = reached.Select(n => new ReachableCell { X = n.Cell.X, Y = n.Cell.Y, Cost = (int)n.Cost }).ToList() };
    }

    /// <summary>Owner mode restricts every operation to the grid owner.</summary>
    static Grid GridRequire(Local db, ulong gridId, string owner)
    {
        var grid = db.Grid.Id.Find(gridId) ?? throw new Exception(GridErrors.NotFound);
        if (grid.Mode == GridModeOwner && grid.OwnerUserId != owner) throw new Exception(GridErrors.NotOwner);
        return grid;
    }

    static void GridAssertInBounds(Grid grid, Coord c)
    {
        if (c.X < 0 || c.Y < 0 || c.X >= grid.Width || c.Y >= grid.Height) throw new Exception(GridErrors.OutOfBounds);
    }

    static List<Coord> GridNeighborsInBounds(Grid grid, Coord c) =>
        GridMath.Neighbors(grid.Kind, c, grid.Connectivity).Where(n => n.X >= 0 && n.Y >= 0 && n.X < grid.Width && n.Y < grid.Height).ToList();

    static CellState? GridFindCellState(Local db, ulong gridId, Coord c)
    {
        foreach (var row in db.CellState.ByGridCell.Filter((gridId, c.X, c.Y))) return row;
        return null;
    }

    static int GridCellCost(Local db, Grid grid, Coord c) => GridFindCellState(db, grid.Id, c)?.Cost ?? grid.DefaultCost;

    static void GridUpsertCellState(Local db, Grid grid, int x, int y, int cost, string? terrain)
    {
        if (cost > GridMaxCellCost) throw new Exception(GridErrors.InvalidCost);
        if ((terrain?.Length ?? 0) > GridMaxTerrainLength) throw new Exception(GridErrors.InvalidTerrain);
        GridAssertInBounds(grid, new Coord(x, y));
        var existing = GridFindCellState(db, grid.Id, new Coord(x, y));
        if (cost == grid.DefaultCost && terrain is null)
        {
            if (existing is { } e) db.CellState.Id.Delete(e.Id);
            return;
        }
        if (existing is { } ex) db.CellState.Id.Update(ex with { Cost = cost, Terrain = terrain });
        else db.CellState.Insert(new CellState { Id = 0, GridId = grid.Id, X = x, Y = y, Cost = cost, Terrain = terrain });
    }

    /// <summary>Blocking entities override the cell cost, so painted cells can't hide them.</summary>
    static Dictionary<Coord, long> GridBuildCostMap(Local db, Grid grid)
    {
        var map = new Dictionary<Coord, long>();
        foreach (var c in db.CellState.GridId.Filter(grid.Id)) map[new Coord(c.X, c.Y)] = c.Cost;
        foreach (var e in db.GridEntity.GridId.Filter(grid.Id))
        {
            if (e.BlocksMovement) map[new Coord(e.X, e.Y)] = -1;
        }
        return map;
    }
}
