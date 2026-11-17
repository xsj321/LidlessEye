// A layered layout for the service map.
//
// A force layout answers "which nodes repel each other" and re-solves it on
// every data change, which is why the same cluster can look different after
// each refresh, and why unrelated services end up side by side. A dependency
// graph has a direction, and the useful thing to show is its depth: entry
// points on the left, what they call to their right, and so on. That is how
// Coroot draws it, and it is what this computes.
//
// Pipeline, in order, and it is Coroot's:
//
//   1. break cycles by dropping the back edges a depth-first walk finds — a
//      service map is full of cycles (a cache that calls back, two services
//      behind each other's retry) and a cycle has no longest path,
//   2. rank the resulting DAG by longest path from a source, so a service sits
//      one column right of everything that feeds it,
//   3. order each column by the mean position of its neighbours in the column
//      before (barycenter), twice, which is enough to remove most crossings at
//      this size,
//   4. spread each column vertically, centred against the tallest one.
//
// Step 1 is the one that decides how the map reads. Collapsing a cycle into a
// single component — the obvious alternative — puts both halves of a mutual
// dependency in the same column, where their edges are vertical and the
// direction is gone. Breaking the loop instead leaves the back edge visible as
// one long line going left, which is exactly the shape a retry loop has.
//
// The gaps come from the canvas rather than from constants, because the graph
// series fits the node bounding box into the drawing rect with one scale for
// both axes: a 9-row graph on a wide dashboard gets scaled up until the rows
// fill the height, and the columns land wherever that scale puts them. Deriving
// the row gap from the height pins the scale at 1, so the column gap below is
// the gap that is drawn.
//
// The caller can then inset the series by the content's margin and get an exact
// 1:1 drawing, which is what lets a node be a card with text of a known size
// rather than a circle that may be scaled up or down.

export interface LayoutNode {
  id: string;
  /** Used to order a column before the barycenter passes. */
  connections: number;
}

export interface LayoutEdge {
  client: string;
  server: string;
}

export interface Point {
  x: number;
  y: number;
}

export interface LayoutOptions {
  /** Canvas width in pixels; the column gap is derived from it. */
  width?: number;
  /** Canvas height in pixels; the row gap is derived from it. */
  height?: number;
  columnGap?: number;
  rowGap?: number;
  /** Floor for the derived column gap: a node card has a width of its own, and
   *  the edge label between two columns needs room to sit in. */
  minColumnGap?: number;
  /** Floor for the derived row gap: a node has a height of its own. */
  minRowGap?: number;
  /** Ceiling for the derived row gap. A graph of three nodes on a tall canvas
   *  would otherwise spread them a screen apart; the drawing code zooms to fit
   *  instead, which keeps the density readable. */
  maxRowGap?: number;
}

/** Bounds the derived gaps: too tight and labels collide, too wide and the
 *  graph turns into long horizontal hairlines. */
const LIMITS = {
  width: 1000,
  height: 640,
  columnGap: [220, 340] as const,
  rowGap: 22,
};

/**
 * Positions for every node in `nodes`, keyed by id. Nodes that appear in no
 * edge share the first column.
 */
export function layeredLayout(
  nodes: LayoutNode[],
  edges: LayoutEdge[],
  options: LayoutOptions = {},
): Map<string, Point> {
  const { columnGap: fixedColumnGap, rowGap: fixedRowGap, minRowGap } = options;
  const indexOf = new Map<string, number>();
  nodes.forEach((node, index) => indexOf.set(node.id, index));
  const count = nodes.length;
  if (count === 0) return new Map();

  const out: number[][] = nodes.map(() => []);
  const incoming: number[][] = nodes.map(() => []);
  const seen = new Set<string>();
  for (const edge of edges) {
    const from = indexOf.get(edge.client);
    const to = indexOf.get(edge.server);
    if (from === undefined || to === undefined || from === to) continue;
    const key = `${from}:${to}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out[from].push(to);
    incoming[to].push(from);
  }

  const rank = longestPath(count, breakCycles(count, out));

  const columns: number[][] = Array.from({ length: Math.max(...rank, 0) + 1 }, () => []);
  for (let node = 0; node < count; node += 1) columns[rank[node]].push(node);

  // Start from the busiest, so the first column reads top-down by importance.
  for (const column of columns) {
    column.sort((a, b) => {
      const byConnections = nodes[b].connections - nodes[a].connections;
      return byConnections !== 0 ? byConnections : nodes[a].id.localeCompare(nodes[b].id);
    });
  }

  // Barycenter: place each node near the average position of the neighbours it
  // has in the adjacent column. Left to right, then right to left.
  const position = new Array<number>(count).fill(0);
  const reposition = () => {
    for (const column of columns) {
      for (let i = 0; i < column.length; i += 1) position[column[i]] = i;
    }
  };
  reposition();

  const orderBy = (column: number[], neighbours: number[][]) => {
    const keyed = column.map((node, i) => {
      const peers = neighbours[node];
      if (peers.length === 0) return { node, key: i, tie: i };
      let sum = 0;
      for (const peer of peers) sum += position[peer];
      return { node, key: sum / peers.length, tie: i };
    });
    keyed.sort((a, b) => (a.key !== b.key ? a.key - b.key : a.tie - b.tie));
    column.splice(0, column.length, ...keyed.map((entry) => entry.node));
    for (let i = 0; i < column.length; i += 1) position[column[i]] = i;
  };

  for (let pass = 0; pass < 2; pass += 1) {
    for (let c = 1; c < columns.length; c += 1) orderBy(columns[c], incoming);
    for (let c = columns.length - 2; c >= 0; c -= 1) orderBy(columns[c], out);
  }

  const tallest = Math.max(...columns.map((column) => column.length));
  const width = options.width && options.width > 0 ? options.width : LIMITS.width;
  const height = options.height && options.height > 0 ? options.height : LIMITS.height;
  // The row gap is what makes the drawing fill the canvas: the graph series
  // fits the node bounding box into the canvas with one scale for both axes, so
  // filling the height puts that scale at 1 and the column gap below lands at
  // the value it was given. A capped row gap would leave the fit to blow the
  // whole graph up, columns included.
  const columnGap =
    fixedColumnGap ??
    clamp(
      width / Math.max(columns.length - 1, 1),
      Math.max(options.minColumnGap ?? 0, LIMITS.columnGap[0]),
      Math.max(options.minColumnGap ?? 0, LIMITS.columnGap[1]),
    );
  const rowGap =
    fixedRowGap ??
    clamp(
      height / Math.max(tallest - 1, 1),
      minRowGap ?? LIMITS.rowGap,
      Math.max(minRowGap ?? 0, options.maxRowGap ?? Number.POSITIVE_INFINITY),
    );

  const positions = new Map<string, Point>();
  columns.forEach((column, columnIndex) => {
    const offset = ((tallest - column.length) * rowGap) / 2;
    column.forEach((node, row) => {
      positions.set(nodes[node].id, {
        x: columnIndex * columnGap,
        y: offset + row * rowGap,
      });
    });
  });
  return positions;
}

function clamp(value: number, low: number, high: number): number {
  return Math.min(high, Math.max(low, value));
}

/**
 * Edges left after a depth-first walk drops every back edge.
 *
 * A back edge is one that points at a node still on the walk's stack, i.e. the
 * edge that closes a cycle. Dropping them is what turns the graph into a DAG
 * while keeping every service in a column of its own. Forward and cross edges
 * pointing at finished nodes are kept: they cannot close a cycle, and they are
 * real dependencies.
 *
 * Iterative on purpose — the graph is user data and a 600-service chain would
 * be 600 frames deep.
 */
function breakCycles(count: number, out: number[][]): number[][] {
  // 0 = unvisited, 1 = on the stack, 2 = finished.
  const state = new Uint8Array(count);
  const dag: number[][] = Array.from({ length: count }, () => []);
  const frames: { node: number; next: number }[] = [];

  for (let root = 0; root < count; root += 1) {
    if (state[root] !== 0) continue;
    state[root] = 1;
    frames.push({ node: root, next: 0 });
    while (frames.length > 0) {
      const frame = frames[frames.length - 1];
      const peers = out[frame.node];
      if (frame.next >= peers.length) {
        state[frame.node] = 2;
        frames.pop();
        continue;
      }
      const peer = peers[frame.next];
      frame.next += 1;
      if (state[peer] === 1) continue;
      dag[frame.node].push(peer);
      if (state[peer] === 0) {
        state[peer] = 1;
        frames.push({ node: peer, next: 0 });
      }
    }
  }
  return dag;
}

/**
 * Longest path from a source, over a DAG: a service ends up one column right of
 * the deepest thing that feeds it, which is the rank Coroot assigns.
 */
function longestPath(count: number, dag: number[][]): number[] {
  const indegree = new Array<number>(count).fill(0);
  for (let from = 0; from < count; from += 1) {
    for (const to of dag[from]) indegree[to] += 1;
  }
  const rank = new Array<number>(count).fill(0);
  const ready: number[] = [];
  for (let node = 0; node < count; node += 1) if (indegree[node] === 0) ready.push(node);
  for (let cursor = 0; cursor < ready.length; cursor += 1) {
    const current = ready[cursor];
    for (const next of dag[current]) {
      rank[next] = Math.max(rank[next], rank[current] + 1);
      indegree[next] -= 1;
      if (indegree[next] === 0) ready.push(next);
    }
  }
  return rank;
}
