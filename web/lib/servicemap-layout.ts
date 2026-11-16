// A layered layout for the service map.
//
// A force layout answers "which nodes repel each other" and re-solves it on
// every data change, which is why the same cluster can look different after
// each refresh, and why unrelated services end up side by side. A dependency
// graph has a direction, and the useful thing to show is its depth: entry
// points on the left, what they call to their right, and so on. That is how
// Coroot draws it, and it is what this computes.
//
// Pipeline, in order:
//
//   1. condense strongly connected components — a service map is full of
//      cycles (a cache that calls back, two services behind each other's
//      retry), and ranking a cycle has no answer,
//   2. rank the resulting DAG by longest path from a source, so a service sits
//      one column right of everything that feeds it,
//   3. order each column by the mean position of its neighbours in the column
//      before (barycenter), twice, which is enough to remove most crossings at
//      this size,
//   4. spread each column vertically, centred against the tallest one.
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

  const comp = stronglyConnected(count, out);
  const compCount = Math.max(...comp, 0) + 1;

  // Condensed DAG.
  const compOut: Set<number>[] = Array.from({ length: compCount }, () => new Set());
  const compIn: number[] = new Array(compCount).fill(0);
  for (let from = 0; from < count; from += 1) {
    for (const to of out[from]) {
      const a = comp[from];
      const b = comp[to];
      if (a === b || compOut[a].has(b)) continue;
      compOut[a].add(b);
      compIn[b] += 1;
    }
  }

  // Longest path from a source: a service sits right of everything feeding it.
  const rank = new Array<number>(compCount).fill(0);
  const ready: number[] = [];
  for (let c = 0; c < compCount; c += 1) if (compIn[c] === 0) ready.push(c);
  for (let cursor = 0; cursor < ready.length; cursor += 1) {
    const current = ready[cursor];
    for (const next of compOut[current]) {
      rank[next] = Math.max(rank[next], rank[current] + 1);
      compIn[next] -= 1;
      if (compIn[next] === 0) ready.push(next);
    }
  }

  const columns: number[][] = Array.from({ length: Math.max(...rank, 0) + 1 }, () => []);
  for (let node = 0; node < count; node += 1) columns[rank[comp[node]]].push(node);

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
 * Tarjan's algorithm, iterative: a recursive version would be simpler but the
 * graph is user data and a 600-node chain would be 600 frames deep.
 *
 * Returns the component id of every node; all nodes of one cycle share an id.
 */
function stronglyConnected(count: number, out: number[][]): number[] {
  const index = new Array<number>(count).fill(-1);
  const low = new Array<number>(count).fill(0);
  const onStack = new Array<boolean>(count).fill(false);
  const comp = new Array<number>(count).fill(-1);
  const stack: number[] = [];
  const frames: { node: number; next: number }[] = [];
  let counter = 0;
  let components = 0;

  for (let root = 0; root < count; root += 1) {
    if (index[root] !== -1) continue;
    frames.push({ node: root, next: 0 });
    while (frames.length > 0) {
      const frame = frames[frames.length - 1];
      const node = frame.node;
      if (frame.next === 0 && index[node] === -1) {
        index[node] = counter;
        low[node] = counter;
        counter += 1;
        stack.push(node);
        onStack[node] = true;
      }

      let descended = false;
      while (frame.next < out[node].length) {
        const peer = out[node][frame.next];
        frame.next += 1;
        if (index[peer] === -1) {
          frames.push({ node: peer, next: 0 });
          descended = true;
          break;
        }
        if (onStack[peer]) low[node] = Math.min(low[node], index[peer]);
      }
      if (descended) continue;

      if (low[node] === index[node]) {
        let member = -1;
        do {
          member = stack.pop() as number;
          onStack[member] = false;
          comp[member] = components;
        } while (member !== node);
        components += 1;
      }
      frames.pop();
      const parent = frames[frames.length - 1];
      if (parent) low[parent.node] = Math.min(low[parent.node], low[node]);
    }
  }
  return comp;
}
