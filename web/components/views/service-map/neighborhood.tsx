"use client";

// The neighbourhood of the selected node: what calls it, what it calls.
//
// This is a different drawing from the big map, not a crop of it. The big map
// answers "what exists"; this one answers "which replica holds the
// connection", so every pod is its own card and owns its container rows rather
// than a workload card hiding its replicas behind a gauge. One DOM subtree per
// pod means a long pod name or a third container can no longer push a row on
// top of its neighbour: the browser stacks them.
//
// Direction is fixed: callers on the left, the selected workload in the
// middle, callees on the right. Every card carries the same rate, protocol and
// latency as the big map, and every card is itself clickable, so walking the
// graph is one hop per click.
//
// Layout is decided in world pixels at scale 1, independent of the drawer's
// width. The drawer asks for a width that fits that world and the view then
// scales to whatever it actually got, which keeps resizing from changing where
// the cards sit.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent, WheelEvent as ReactWheelEvent } from "react";
import { Maximize2, Minus, Plus } from "lucide-react";

import { KIND, edgeWeight, formatRate, nodeLabel, nodeMeta } from "@/lib/servicemap-graph";
import type { Box } from "@/lib/servicemap-graph";
import type { ServiceMapEdge, ServiceMapNode } from "@/lib/types";
import { Button } from "@/components/ui/button";

import { EdgeLayer, edgeKey } from "./edge-layer";
import {
  NEIGHBOR_CARD,
  NeighborCard,
  neighborCardHeight,
  neighborRowCenter,
  neighborRowLeft,
  neighborRowRight,
} from "./neighbor-card";
import type { NeighborContainer } from "./neighbor-card";

/** How many peers a column shows before it says "+n more". */
const MAX_PEERS = 6;
/** Containers drawn inside one pod card, before a "+n more" row. */
const MAX_CONTAINERS = 6;
/** Pods drawn for one workload; the rest collapse into an overflow card. */
const MAX_PODS = 6;

const SIZE = {
  pad: 20,
  /** The strip above the cards that holds the CALLERS / CALLEES caption. */
  headerRow: 16,
  /** Room between two columns for the edge label. */
  gapX: 92,
  cardGapY: 16,
  minCard: 214,
  maxCard: 300,
};

const ZOOM = { min: 0.5, max: 2.4, step: 1.22 };
/** Width reserved outside a column for the arcs between two of its cards. */
const ARC_ROOM = 72;
/** The first lane's bow, and the gap between lanes. */
const ARC_DEPTH = 24;
const ARC_STEP = 7;
/** How many same-side pairs get an arc before the rest are only counted. */
const MAX_ARCS = 6;
const MAX_VIEW_HEIGHT = 440;
const MIN_VIEW_HEIGHT = 260;
/** Breathing room kept around the fitted graph, in viewport pixels. */
const FIT_MARGIN = 22;

export interface NeighborhoodProps {
  node: ServiceMapNode;
  nodes: Map<string, ServiceMapNode>;
  edges: ServiceMapEdge[];
  windowSec: number;
  onSelect: (id: string) => void;
  /** The drawer uses this to choose a width that fits the drawing at scale 1. */
  onPreferredWidth?: (width: number) => void;
}

function clamp(value: number, low: number, high: number): number {
  return Math.min(high, Math.max(low, value));
}

/** One peer, with every edge that points at it. */
interface PeerGroup {
  id: string;
  edges: ServiceMapEdge[];
  weight: number;
}

/**
 * One card in the drawing.
 *
 * `key` is what identifies a card, not the node: a workload that both calls and
 * is called gets one card per side, because "they call you" and "you call them"
 * carry different container rows and rates. Keying by node id made the second
 * card overwrite the first, which left a hole in one column and drew that
 * side's hops to a card on the wrong side of the selected workload.
 */
interface Slot {
  key: string;
  /** Workload id, kept so a pod card still walks the graph to its workload. */
  id: string;
  side: "up" | "self" | "down";
  /** UID of one replica; empty for traffic that could not be pinned to a pod. */
  podUid?: string;
  podName?: string;
  /** Count of replicas hidden behind this overflow card. */
  morePods?: number;
}

interface Layout {
  /** Workload cards — the only things a label must avoid. */
  cards: Map<string, Box>;
  /**
   * Edge endpoints. Every container row has two point-sized anchors: `<key>@out`
   * on the row's right edge and `<key>@in` on its left, matching the two ports
   * drawn on that row. A curve leaves a source row's `@out` and lands on a
   * target row's `@in`, so it runs container-to-container at the row's own
   * height and terminates on the container, not on the workload card.
   * Workloads whose edge could not be attributed to a container get `<id>@out`
   * / `<id>@in` on the card's header edge.
   */
  anchors: Map<string, Box>;
  rowNames: Map<string, string>;
  /** Cards in reading order: callers, the selected workload, callees. */
  slots: Slot[];
  /** Caption above each side, so "upstream" and "downstream" read at a glance. */
  captions: { x: number; w: number; label: string }[];
  height: number;
  width: number;
}

/** A point-sized box whose centre is (x, y): edgePath reads its edges. */
function port(x: number, y: number): Box {
  return { x: x - 0.5, y: y - 0.5, w: 1, h: 1 };
}

/** Strip the `@in` / `@out` suffix back to a row or workload key. */
function anchorBase(key: string): string {
  return key.replace(/@(in|out)$/, "");
}

/** Pod UID carried by an edge end, or empty when the agent could not tell. */
function podUidOf(edge: ServiceMapEdge, end: "client" | "server"): string {
  return (end === "client" ? edge.client_pod : edge.server_pod) ?? "";
}

/** Which end of an edge is the pod that owns `id` on a given side. */
function podUidForCard(edge: ServiceMapEdge, id: string, side: Slot["side"]): string {
  if (side === "up") return podUidOf(edge, "client");
  if (side === "down") return podUidOf(edge, "server");
  return edge.server === id ? podUidOf(edge, "server") : podUidOf(edge, "client");
}

function instanceName(node: ServiceMapNode | undefined, uid: string): string {
  const instance = node?.instances?.find((item) => item.uid === uid);
  return instance?.name ?? uid;
}

function slotLabel(node: ServiceMapNode | undefined, slot: Slot): string {
  if (slot.morePods) return `+${slot.morePods} more pods`;
  return slot.podName ?? (node ? nodeLabel(node) : slot.id);
}

function slotMeta(node: ServiceMapNode | undefined, slot: Slot): string {
  if (!slot.podName) return node ? nodeMeta(node) : "";
  const owner = node?.workload || node?.service || node?.id || "";
  return [node?.namespace, owner].filter(Boolean).join(" · ") || "pod";
}

function groupPeers(edges: ServiceMapEdge[], peerOf: (edge: ServiceMapEdge) => string): PeerGroup[] {
  const groups = new Map<string, PeerGroup>();
  for (const edge of edges) {
    const id = peerOf(edge);
    const group = groups.get(id) ?? { id, edges: [], weight: 0 };
    group.edges.push(edge);
    group.weight += edgeWeight(edge);
    groups.set(id, group);
  }
  return [...groups.values()].sort((a, b) => b.weight - a.weight);
}

/**
 * Collapse the edges that land on one pair of container rows.
 *
 * Several protocols between the same two containers are one line: the panel
 * answers "who talks to whom", and three stacked curves to the same row would
 * only make the rate unreadable. Every protocol name survives in the label,
 * though — that is the part the payload sniffer added and the port guess could
 * not know. RTT is weighted by connections, so a busy slow edge is not
 * averaged away by an idle fast one.
 */
function fold(
  edges: ServiceMapEdge[],
  client: string,
  server: string,
  clientContainer: string,
  serverContainer: string,
): ServiceMapEdge {
  let connections = 0;
  let retransmits = 0;
  let tx = 0;
  let rx = 0;
  let rttWeighted = 0;
  let rttWeight = 0;
  const protocols = new Set<string>();
  for (const edge of edges) {
    const count = edge.connections ?? 0;
    connections += count;
    retransmits += edge.retransmits ?? 0;
    tx += edge.tx_bytes ?? 0;
    rx += edge.rx_bytes ?? 0;
    const weight = Math.max(count, 1);
    rttWeighted += (edge.rtt_us ?? 0) * weight;
    rttWeight += (edge.rtt_us ?? 0) > 0 ? weight : 0;
    for (const name of (edge.protocol || "tcp").split("+").filter(Boolean)) {
      protocols.add(name);
    }
  }
  protocols.delete("tcp");
  return {
    client,
    server,
    protocol: [...protocols].join("+"),
    client_container: clientContainer,
    server_container: serverContainer,
    connections,
    retransmits,
    tx_bytes: tx,
    rx_bytes: rx,
    rtt_us: rttWeight > 0 ? Math.round(rttWeighted / rttWeight) : 0,
  };
}

export function Neighborhood({
  node,
  nodes,
  edges,
  windowSec,
  onSelect,
  onPreferredWidth,
}: NeighborhoodProps) {
  const [available, setAvailable] = useState(0);
  const [view, setView] = useState({ x: 0, y: 0, k: 1 });
  const viewport = useRef<HTMLDivElement | null>(null);
  const observer = useRef<ResizeObserver | null>(null);
  const attach = useCallback((element: HTMLDivElement | null) => {
    viewport.current = element;
    observer.current?.disconnect();
    observer.current = null;
    if (!element) return;
    setAvailable(element.clientWidth);
    const created = new ResizeObserver((entries) => {
      const box = entries[0]?.contentRect;
      if (box) setAvailable(box.width);
    });
    created.observe(element);
    observer.current = created;
  }, []);

  const inbound = useMemo(
    () =>
      edges.filter((edge) => edge.server === node.id).sort((a, b) => edgeWeight(b) - edgeWeight(a)),
    [edges, node.id],
  );
  const outbound = useMemo(
    () =>
      edges.filter((edge) => edge.client === node.id).sort((a, b) => edgeWeight(b) - edgeWeight(a)),
    [edges, node.id],
  );

  const allUp = useMemo(() => groupPeers(inbound, (edge) => edge.client), [inbound]);
  const allDown = useMemo(() => groupPeers(outbound, (edge) => edge.server), [outbound]);
  const upGroups = useMemo(() => allUp.slice(0, MAX_PEERS), [allUp]);
  const downGroups = useMemo(() => allDown.slice(0, MAX_PEERS), [allDown]);
  // A card per pod, not per workload. The agent's edge list used to carry only
  // the workload id, so a two-replica deployment drew one card and the traffic
  // of both pods was folded into it. Edges now carry pod UIDs; each replica gets
  // its own card and container rows. A fallback card remains for traffic the
  // agent could not attribute (services, external peers, old data).
  const slots = useMemo<Slot[]>(() => {
    const pushFor = (out: Slot[], id: string, side: Slot["side"], related: ServiceMapEdge[]) => {
      const info = nodes.get(id);
      if (!info) return;
      const instances = (info.instances ?? []).filter((item) => item.uid);
      const known = new Set(instances.map((item) => item.uid));
      const prefix = side === "self" ? "self" : `${side}:${id}`;
      const unattributed = related.some((edge) => {
        const uid = podUidForCard(edge, id, side);
        return !uid || !known.has(uid);
      });
      if (instances.length === 0 || unattributed) {
        out.push({ key: prefix, id, side });
      }
      for (const instance of instances.slice(0, MAX_PODS)) {
        out.push({
          key: `${prefix}::pod:${instance.uid}`,
          id,
          side,
          podUid: instance.uid,
          podName: instance.name,
        });
      }
      if (instances.length > MAX_PODS) {
        out.push({
          key: `${prefix}::more`,
          id,
          side,
          morePods: instances.length - MAX_PODS,
        });
      }
    };

    const out: Slot[] = [];
    for (const group of upGroups) pushFor(out, group.id, "up", group.edges);
    pushFor(out, node.id, "self", [...inbound, ...outbound]);
    for (const group of downGroups) pushFor(out, group.id, "down", group.edges);
    return out;
  }, [upGroups, downGroups, node.id, nodes, inbound, outbound]);

  /** The card(s) a node has, so a peer-to-peer line can pick a side. */
  const slotsOf = useMemo(() => {
    const map = new Map<string, { up?: string; down?: string }>();
    for (const slot of slots) {
      if (slot.side === "self" || slot.morePods) continue;
      const entry = map.get(slot.id) ?? {};
      // One representative per side is enough for same-side arcs; the first
      // slot is the fallback (or the busiest pod) and stays stable.
      entry[slot.side] ??= slot.key;
      map.set(slot.id, entry);
    }
    return map;
  }, [slots]);

  /** Pick the card an edge end lands on, falling back when it has no pod UID. */
  const slotKeyOf = useCallback(
    (owner: string, side: Slot["side"], podUid: string) => {
      const prefix = side === "self" ? "self" : `${side}:${owner}`;
      if (podUid) {
        const match = slots.find((slot) => slot.podUid === podUid && slot.side === side);
        if (match) return match.key;
      }
      return prefix;
    },
    [slots],
  );

  const rowId = useCallback((owner: string, name: string) => `${owner}::${name}`, []);

  /** Which column an edge end belongs to, including peer-to-peer arcs. */
  const cardSideOf = useCallback(
    (edge: ServiceMapEdge, end: "client" | "server"): Slot["side"] => {
      const owner = end === "client" ? edge.client : edge.server;
      if (owner === node.id) return "self";
      if (edge.server === node.id && end === "client") return "up";
      if (edge.client === node.id && end === "server") return "down";
      const entry = slotsOf.get(owner);
      if (entry?.up && !entry?.down) return "up";
      if (entry?.down && !entry?.up) return "down";
      // A same-side arc: the other end has the same column, and is the only
      // way to tell an upstream pair from a downstream one.
      const other = slotsOf.get(end === "client" ? edge.server : edge.client);
      return other?.up ? "up" : "down";
    },
    [node.id, slotsOf],
  );

  // Containers worth a row, in reading order: the ones that actually carried
  // traffic in this window first (heaviest at the top), then the rest of what
  // the pod list declares. A long tail collapses into one "+n more" row rather
  // than making a single card taller than the viewport.
  const containers = useMemo(() => {
    const traffic = new Map<string, Map<string, number>>();
    const cardOf = (id: string, podUid: string, side: Slot["side"]) => {
      const match = podUid
        ? slots.find((slot) => slot.id === id && slot.side === side && slot.podUid === podUid)
        : undefined;
      return (
        match?.key ??
        slots.find((slot) => slot.id === id && slot.side === side && !slot.podUid)?.key
      );
    };
    const add = (
      id: string,
      podUid: string,
      side: Slot["side"],
      name: string | undefined,
      weight: number,
    ) => {
      const owner = cardOf(id, podUid, side);
      if (!name || !owner) return;
      const slot = traffic.get(owner) ?? new Map<string, number>();
      slot.set(name, (slot.get(name) ?? 0) + weight);
      traffic.set(owner, slot);
    };
    for (const edge of edges) {
      const weight = edgeWeight(edge);
      add(
        edge.client,
        podUidOf(edge, "client"),
        cardSideOf(edge, "client"),
        edge.client_container,
        weight,
      );
      add(
        edge.server,
        podUidOf(edge, "server"),
        cardSideOf(edge, "server"),
        edge.server_container,
        weight,
      );
    }
    const out = new Map<string, { names: string[]; extra: number }>();
    for (const slot of slots) {
      const active = [...(traffic.get(slot.key)?.entries() ?? [])]
        .sort((a, b) => b[1] - a[1])
        .map(([name]) => name);
      const declared = (nodes.get(slot.id)?.containers ?? []).filter(
        (name) => name && !active.includes(name),
      );
      const all = [...active, ...declared];
      out.set(slot.key, {
        names: all.slice(0, MAX_CONTAINERS),
        extra: Math.max(0, all.length - MAX_CONTAINERS),
      });
    }
    return out;
  }, [edges, nodes, slots, cardSideOf]);

  // Which columns carry same-side arcs. The layout reserves room outside those
  // columns for them, so an arc never has to be clipped at the drawing's edge.
  const arcSides = useMemo(() => {
    let up = false;
    let down = false;
    for (const edge of edges) {
      if (edge.client === node.id || edge.server === node.id) continue;
      const a = slotsOf.get(edge.client);
      const b = slotsOf.get(edge.server);
      if (!a || !b) continue;
      if (a.up && b.up) up = true;
      else if (a.down && b.down) down = true;
    }
    return { up, down };
  }, [edges, node.id, slotsOf]);

  const layout = useMemo<Layout>(() => {
    const rowCountOf = (id: string) => {
      const info = containers.get(id);
      return (info?.names.length ?? 0) + (info?.extra ? 1 : 0);
    };
    const rowIndexOf = (id: string, name: string | undefined): number => {
      if (!name) return -1;
      return (containers.get(id)?.names ?? []).indexOf(name);
    };
    const heightOf = (id: string) => neighborCardHeight(rowCountOf(id));

    const textWidth = Math.max(
      12,
      ...slots.flatMap((slot) => {
        const meta = nodes.get(slot.id);
        return [
          slotLabel(meta, slot).length * 6.6,
          slotMeta(meta, slot).length * 5.4,
          ...(containers.get(slot.key)?.names ?? []).map((name) => name.length * 6.1 + 46),
        ];
      }),
    );
    const cardWidth = clamp(Math.round(textWidth + 48), SIZE.minCard, SIZE.maxCard);

    // Only the columns that exist take width: a node with only callees is a
    // two-column drawing, not a three-column one with an empty left edge.
    const columns: Slot[][] = [
      slots.filter((slot) => slot.side === "up"),
      slots.filter((slot) => slot.side === "self"),
      slots.filter((slot) => slot.side === "down"),
    ].filter((column) => column.length > 0);

    // Room outside a column for the arcs between two cards on that side.
    const padLeft = SIZE.pad + (arcSides.up ? ARC_ROOM : 0);
    const padRight = SIZE.pad + (arcSides.down ? ARC_ROOM : 0);
    const xOf = (index: number) => padLeft + index * (cardWidth + SIZE.gapX);
    const span =
      columns.length * cardWidth + Math.max(0, columns.length - 1) * SIZE.gapX;
    const width = padLeft + span + padRight;

    const selfColumn = Math.max(
      0,
      columns.findIndex((column) => column[0]?.side === "self"),
    );
    const selfSlots = slots.filter((slot) => slot.side === "self");
    // Self pods are placed first, stacked in the middle column. Every peer is
    // then positioned relative to the row of the replica it actually talked
    // to, and the whole drawing is shifted down afterwards.
    const selfY = new Map<string, number>();
    let selfBottom = 0;
    for (const slot of selfSlots) {
      selfY.set(slot.key, selfBottom);
      selfBottom += heightOf(slot.key) + SIZE.cardGapY;
    }

    // Traffic with no container attribution attaches to the header edge, so
    // both the alignment and the anchor use that same height.
    const headerMid = NEIGHBOR_CARD.header / 2;
    /** Y (in the provisional frame) of the self row a peer should line up to. */
    const selfRowY = (slotKey: string, container: string | undefined) => {
      const index = rowIndexOf(slotKey, container);
      return index >= 0 ? neighborRowCenter(index) : headerMid;
    };
    /** Y of the peer row that carries the edge, relative to the peer's card. */
    const peerRowY = (slotKey: string, container: string | undefined) => {
      const index = rowIndexOf(slotKey, container);
      return index >= 0 ? neighborRowCenter(index) : headerMid;
    };

    const heaviest = (candidates: ServiceMapEdge[]) =>
      candidates.reduce(
        (best, edge) => (edgeWeight(edge) > edgeWeight(best) ? edge : best),
        candidates[0],
      );

    const cards = new Map<string, Box>();
    for (const slot of selfSlots) {
      cards.set(slot.key, {
        x: xOf(selfColumn),
        y: selfY.get(slot.key) ?? 0,
        w: cardWidth,
        h: heightOf(slot.key),
      });
    }

    columns.forEach((column, columnIndex) => {
      if (column[0]?.side === "self") return;
      const fromPeer = column[0]?.side === "up";
      const groups = fromPeer ? upGroups : downGroups;
      // Line each peer's connecting container up with the self row it talks to,
      // so the hop is a straight horizontal line between two ports. Peers that
      // target the same row are then pushed apart to keep them from touching.
      const placed = column
        .map((slot) => {
          const group = groups.find((candidate) => candidate.id === slot.id);
          if (!group) return null;
          // Align this replica to the row it actually used. Using the busiest
          // edge of the whole workload put every pod card on the same row when
          // a sidecar and the app carried different connections.
          const side = fromPeer ? "up" : "down";
          const ownEdges = group.edges.filter(
            (candidate) => podUidForCard(candidate, slot.id, side) === (slot.podUid ?? ""),
          );
          const edge = heaviest(ownEdges.length > 0 ? ownEdges : group.edges);
          const selfContainer = fromPeer ? edge.server_container : edge.client_container;
          const peerContainer = fromPeer ? edge.client_container : edge.server_container;
          const selfKey = slotKeyOf(
            node.id,
            "self",
            podUidOf(edge, fromPeer ? "server" : "client"),
          );
          return {
            slot,
            wanted:
              (selfY.get(selfKey) ?? 0) +
              selfRowY(selfKey, selfContainer) -
              peerRowY(slot.key, peerContainer),
            h: heightOf(slot.key),
          };
        })
        .filter((item): item is { slot: Slot; wanted: number; h: number } => item != null)
        .sort((a, b) => a.wanted - b.wanted);

      let bottom = Number.NEGATIVE_INFINITY;
      for (const item of placed) {
        const y = Math.max(item.wanted, bottom + SIZE.cardGapY);
        cards.set(item.slot.key, { x: xOf(columnIndex), y, w: cardWidth, h: item.h });
        bottom = y + item.h;
      }
    });

    // Shift everything so the top of the drawing clears the caption strip.
    let minY = Number.POSITIVE_INFINITY;
    let maxY = Number.NEGATIVE_INFINITY;
    for (const card of cards.values()) {
      minY = Math.min(minY, card.y);
      maxY = Math.max(maxY, card.y + card.h);
    }
    const top = SIZE.pad + SIZE.headerRow;
    const shift = minY === Number.POSITIVE_INFINITY ? 0 : top - minY;
    for (const card of cards.values()) card.y += shift;
    const height = maxY === Number.NEGATIVE_INFINITY ? top : maxY - minY + top + SIZE.pad;

    const anchors = new Map<string, Box>();
    const rowNames = new Map<string, string>();
    for (const slot of slots) {
      const card = cards.get(slot.key);
      if (!card) continue;
      // Fallback for traffic the agent could not attribute to a container:
      // the header edge, where the workload's name is, not the middle of the
      // body — a curve there would strike through the container rows.
      const mid = card.y + headerMid;
      anchors.set(`${slot.key}@out`, port(card.x + card.w, mid));
      anchors.set(`${slot.key}@in`, port(card.x, mid));
      (containers.get(slot.key)?.names ?? []).forEach((name, index) => {
        const key = rowId(slot.key, name);
        const centre = card.y + neighborRowCenter(index);
        // A container's endpoints are its **own** pane edges, not the card's
        // borders: the row is what holds the connection, so the line has to
        // land on the row. The drawer keeps these two numbers equal to the
        // dots NeighborCard draws (NEIGHBOR_CARD.indent / rightPad).
        anchors.set(`${key}@out`, port(card.x + neighborRowRight(card.w), centre));
        anchors.set(`${key}@in`, port(card.x + neighborRowLeft(), centre));
        rowNames.set(key, name);
      });
    }

    const captions: Layout["captions"] = columns
      .map((column, index) =>
        column[0]?.side === "self" || column.length === 0
          ? null
          : {
              x: xOf(index),
              w: cardWidth,
              label: column[0].side === "up" ? "CALLERS" : "CALLEES",
            },
      )
      .filter((caption): caption is Layout["captions"][number] => caption != null);

    return {
      cards,
      anchors,
      rowNames,
      slots,
      captions,
      height,
      width,
    };
  }, [upGroups, downGroups, node.id, nodes, containers, rowId, slots, slotKeyOf, arcSides]);

  // One line per pair of container rows. A client leaves its row's right port
  // (`@out`) and a server is entered at its row's left port (`@in`), so the
  // curve is drawn from one container to the other rather than from card to
  // card. An edge with no container stays on its workload's card centre.
  const endpoint = useCallback(
    (owner: string, container: string | undefined, side: "out" | "in") => {
      const candidate = container ? rowId(owner, container) : "";
      // An edge the agent could not attribute down to a container stays on the
      // workload card instead of disappearing from the drawing.
      const base = candidate && layout.anchors.has(`${candidate}@${side}`) ? candidate : owner;
      return `${base}@${side}`;
    },
    [layout.anchors, rowId],
  );

  /** Fold a group of raw edges by the pair of ports they land on. */
  const foldByPorts = useCallback(
    (edges: ServiceMapEdge[], clientOf: (edge: ServiceMapEdge) => string, serverOf: (edge: ServiceMapEdge) => string) => {
      const pairs = new Map<string, ServiceMapEdge[]>();
      for (const edge of edges) {
        const client = clientOf(edge);
        const server = serverOf(edge);
        if (!layout.anchors.has(client) || !layout.anchors.has(server)) continue;
        const key = `${client}|${server}`;
        const slot = pairs.get(key) ?? [];
        slot.push(edge);
        pairs.set(key, slot);
      }
      const out: ServiceMapEdge[] = [];
      for (const [key, matching] of pairs) {
        const [client, server] = key.split("|");
        out.push(
          fold(
            matching,
            client,
            server,
            layout.rowNames.get(anchorBase(client)) ?? "",
            layout.rowNames.get(anchorBase(server)) ?? "",
          ),
        );
      }
      return out;
    },
    [layout.anchors, layout.rowNames],
  );

  // The star: what calls the selected workload and what it calls. This is the
  // question the panel exists to answer, so these hops are labelled and drawn
  // above the cards.
  const shown = useMemo(() => {
    const out: ServiceMapEdge[] = [];
    const emit = (group: PeerGroup, fromPeer: boolean) => {
      const side = fromPeer ? "up" : "down";
      for (const edge of group.edges) {
        const peer = slotKeyOf(group.id, side, podUidOf(edge, fromPeer ? "client" : "server"));
        const self = slotKeyOf(node.id, "self", podUidOf(edge, fromPeer ? "server" : "client"));
        if (!layout.cards.has(peer) || !layout.cards.has(self)) continue;
        out.push(
          ...foldByPorts(
            [edge],
            () => endpoint(fromPeer ? peer : self, edge.client_container, "out"),
            () => endpoint(fromPeer ? self : peer, edge.server_container, "in"),
          ),
        );
      }
    };
    for (const group of upGroups) emit(group, true);
    for (const group of downGroups) emit(group, false);
    return out;
  }, [upGroups, downGroups, layout.cards, endpoint, foldByPorts, slotKeyOf, node.id]);

  // Arcs between two peers on the same side.
  //
  // Both are on screen and they talk to each other, so the drawing used to make
  // them look unrelated. Only same-column pairs are drawn, and they bow outside
  // the column: a caller→callee pair already has both ends connected to the
  // middle card, and a curve through it would read as "both talk to the
  // selected workload" — which is a different, wrong claim.
  //
  // The arcs are nested by rank instead of sharing one radius: a dozen curves
  // at the same distance collapse into a single dark band with no way to tell
  // which two cards a line joins. The heaviest few get a lane each.
  const mesh = useMemo(() => {
    const pairs = new Map<
      string,
      { client: string; server: string; edges: ServiceMapEdge[]; weight: number; side: "up" | "down" }
    >();
    for (const edge of edges) {
      if (edge.client === node.id || edge.server === node.id) continue;
      const a = slotsOf.get(edge.client);
      const b = slotsOf.get(edge.server);
      if (!a || !b) continue;
      const side = a.up && b.up ? "up" : a.down && b.down ? "down" : null;
      if (!side) continue;
      const clientSlot = slotKeyOf(edge.client, side, podUidOf(edge, "client"));
      const serverSlot = slotKeyOf(edge.server, side, podUidOf(edge, "server"));
      if (clientSlot === serverSlot) continue;
      // One arc per pair: a curve each way would draw over itself, and the arc
      // is deliberately undirected.
      const key = [clientSlot, serverSlot].sort().join("|");
      const weight = edgeWeight(edge);
      const entry = pairs.get(key);
      if (!entry) {
        pairs.set(key, { client: clientSlot, server: serverSlot, edges: [edge], weight, side });
      } else {
        entry.edges.push(edge);
        entry.weight += weight;
      }
    }

    const ranked = [...pairs.values()].sort((a, b) => b.weight - a.weight);
    const drawn = ranked.slice(0, MAX_ARCS);
    const out: ServiceMapEdge[] = [];
    const depths = new Map<string, number>();
    for (const [rank, entry] of drawn.entries()) {
      // Both ends land on the column's **outer** edge and the curve bows the
      // same way: a caller column is bracketed on its left, a callee column on
      // its right. `out` on one end and `in` on the other would draw the arc
      // straight through the cards it is supposed to connect.
      const anchorSide = entry.side === "up" ? "in" : "out";
      const folded = foldByPorts(
        entry.edges,
        (edge) => endpoint(entry.client, edge.client_container, anchorSide),
        (edge) => endpoint(entry.server, edge.server_container, anchorSide),
      );
      for (const edge of folded) {
        depths.set(edgeKey(edge), ARC_DEPTH + rank * ARC_STEP);
        out.push(edge);
      }
    }
    return { edges: out, depths, hidden: ranked.length - drawn.length };
  }, [edges, node.id, slotsOf, slotKeyOf, endpoint, foldByPorts]);

  /** Callers bow left, callees bow right: away from the selected workload. */
  const bowSide = useCallback(
    (edge: ServiceMapEdge): { side: "left" | "right"; depth: number } => ({
      side: anchorBase(edge.client).startsWith("up:") ? "left" : "right",
      depth: mesh.depths.get(edgeKey(edge)) ?? ARC_DEPTH,
    }),
    [mesh.depths],
  );

  /** Connections per row, and which ports actually carry an edge. */
  const { perRow, usedPorts, usedCardPorts } = useMemo(() => {
    const totals = new Map<string, number>();
    const ports = new Map<string, { in?: boolean; out?: boolean }>();
    const cardPorts = new Map<string, { in?: boolean; out?: boolean }>();
    const mark = (anchor: string) => {
      const side = anchor.endsWith("@out") ? "out" : anchor.endsWith("@in") ? "in" : null;
      if (!side) return;
      const base = anchorBase(anchor);
      // A container port for a row, or the header port of a workload whose
      // traffic could not be attributed to one.
      const target = layout.rowNames.has(base)
        ? ports
        : layout.cards.has(base)
          ? cardPorts
          : null;
      if (!target) return;
      const state = target.get(base) ?? {};
      state[side] = true;
      target.set(base, state);
    };
    for (const edge of [...shown, ...mesh.edges]) {
      const client = anchorBase(edge.client);
      const server = anchorBase(edge.server);
      totals.set(client, (totals.get(client) ?? 0) + (edge.connections ?? 0));
      totals.set(server, (totals.get(server) ?? 0) + (edge.connections ?? 0));
      mark(edge.client);
      mark(edge.server);
    }
    return { perRow: totals, usedPorts: ports, usedCardPorts: cardPorts };
  }, [shown, mesh, layout.rowNames, layout.cards]);

  const reportWidth = useCallback(
    (width: number) => onPreferredWidth?.(width),
    [onPreferredWidth],
  );
  useEffect(() => {
    reportWidth(layout.width + 64);
  }, [layout.width, reportWidth]);

  const viewHeight = clamp(Math.round(layout.height), MIN_VIEW_HEIGHT, MAX_VIEW_HEIGHT);
  const fit = useCallback(() => {
    if (available <= 0) return;
    // Keep a margin on all four sides. At 10px a card's border and an edge
    // label sat exactly on the clipping edge and read as cut off; the graph
    // has to look like it fits, not like it was cropped.
    const k = clamp(
      Math.min(
        (available - FIT_MARGIN * 2) / layout.width,
        (viewHeight - FIT_MARGIN * 2) / layout.height,
      ),
      ZOOM.min,
      1,
    );
    setView({
      k,
      x: (available - layout.width * k) / 2,
      y: (viewHeight - layout.height * k) / 2,
    });
  }, [available, layout.width, layout.height, viewHeight]);

  // The first fit, and every fit after the drawer or the topology changes size.
  // A zoom is left alone while the payload refreshes with the same geometry.
  const lastFit = useRef("");
  useEffect(() => {
    const signature = `${Math.round(available)}x${Math.round(layout.width)}x${Math.round(layout.height)}`;
    if (signature === lastFit.current) return;
    lastFit.current = signature;
    fit();
  }, [fit, available, layout.width, layout.height]);

  // Wheel is attached by hand: React's synthetic wheel is passive, and a
  // passive listener cannot stop the page scrolling under the drawer.
  useEffect(() => {
    const element = viewport.current;
    if (!element) return;
    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      const rect = element.getBoundingClientRect();
      const px = event.clientX - rect.left;
      const py = event.clientY - rect.top;
      setView((previous) => {
        const k = clamp(previous.k * Math.exp(-event.deltaY * 0.0015), ZOOM.min, ZOOM.max);
        const ratio = k / previous.k;
        return { k, x: px - (px - previous.x) * ratio, y: py - (py - previous.y) * ratio };
      });
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, []);

  const zoomBy = (factor: number) => {
    setView((previous) => {
      const k = clamp(previous.k * factor, ZOOM.min, ZOOM.max);
      const ratio = k / previous.k;
      const px = available / 2;
      const py = viewHeight / 2;
      return { k, x: px - (px - previous.x) * ratio, y: py - (py - previous.y) * ratio };
    });
  };

  // Panning starts from anywhere on the surface, cards included: most of this
  // drawing is cards, so "drag the map" and "drag a card" are the same gesture
  // to the person doing it. The capture is taken only once the pointer has
  // actually moved, because capturing on pointerdown retargets the click to
  // this element and the card would never walk the graph.
  const drag = useRef<{ x: number; y: number; vx: number; vy: number; moved: boolean } | null>(
    null,
  );
  const onPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    const target = event.target as Element | null;
    if (target?.closest("[data-map-controls]")) return;
    drag.current = { x: event.clientX, y: event.clientY, vx: view.x, vy: view.y, moved: false };
  };
  const onPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    const state = drag.current;
    if (!state) return;
    const dx = event.clientX - state.x;
    const dy = event.clientY - state.y;
    if (!state.moved) {
      // A few pixels of slop keeps a shaky click a click.
      if (Math.abs(dx) + Math.abs(dy) < 4) return;
      state.moved = true;
      try {
        event.currentTarget.setPointerCapture(event.pointerId);
      } catch {
        /* the pointer may already be gone */
      }
    }
    setView((previous) => ({ ...previous, x: state.vx + dx, y: state.vy + dy }));
  };
  const onPointerUp = () => {
    drag.current = null;
  };

  let cpuMax = 1;
  for (const slot of layout.slots) cpuMax = Math.max(cpuMax, nodes.get(slot.id)?.cpu_ns ?? 0);

  const hiddenIn = allUp.length - upGroups.length;
  const hiddenOut = allDown.length - downGroups.length;
  const rate = (connections: number) => (windowSec > 0 ? Math.max(0, connections) / windowSec : 0);

  return (
    <div ref={attach} data-testid="map-neighborhood" data-layout-width={layout.width}>
      <div className="mb-1 flex items-center justify-between text-[9px] uppercase tracking-wide text-muted-foreground">
        <span>Neighbours</span>
        <span className="flex items-center gap-1 normal-case tracking-normal">
          <span className="mr-1 font-mono">
            {inbound.length} in · {outbound.length} out
          </span>
          <span data-map-controls className="flex items-center gap-0.5">
            <Button
              variant="ghost"
              size="xs"
              aria-label="Zoom neighbourhood in"
              data-testid="map-neighborhood-zoom-in"
              onClick={() => zoomBy(ZOOM.step)}
            >
              <Plus />
            </Button>
            <Button
              variant="ghost"
              size="xs"
              aria-label="Zoom neighbourhood out"
              data-testid="map-neighborhood-zoom-out"
              onClick={() => zoomBy(1 / ZOOM.step)}
            >
              <Minus />
            </Button>
            <Button
              variant="ghost"
              size="xs"
              aria-label="Fit neighbourhood to view"
              data-testid="map-neighborhood-fit"
              onClick={fit}
            >
              <Maximize2 />
            </Button>
          </span>
        </span>
      </div>
      {/* `select-none` is not cosmetic: dragging the map used to select the
          card labels, and the second drag started a native text drag, which
          Chromium turns into a search for the selected words. A pannable
          surface must not be a text surface. `onDragStart` covers the
          pointer's own drag gesture (icons, links) for the same reason. */}
      {available > 0 ? (
        <div
          className="relative cursor-grab touch-none overflow-clip rounded-md border bg-background/30 select-none active:cursor-grabbing"
          style={{ height: viewHeight }}
          data-zoom={view.k.toFixed(3)}
          draggable={false}
          onDragStart={(event) => event.preventDefault()}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerUp}
          onWheel={(event: ReactWheelEvent<HTMLDivElement>) => event.preventDefault()}
        >
          <div
            className="absolute left-0 top-0 origin-top-left"
            style={{
              width: layout.width,
              height: layout.height,
              transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})`,
            }}
          >
            {/* Upstream and downstream are captioned once, above the cards,
                so a glance tells which way the traffic flows without reading
                a legend. */}
            {layout.captions.map((caption) => (
              <div
                key={caption.label}
                className="pointer-events-none absolute text-[8.5px] font-medium uppercase tracking-[0.14em] text-muted-foreground/60"
                style={{ left: caption.x, top: SIZE.pad - 4, width: caption.w }}
              >
                {caption.label}
              </div>
            ))}
            {/* Peer-to-peer traffic first, so it sits behind both the cards and
                the hops to the selected workload: it is context, not the
                question the panel was opened to answer. */}
            <EdgeLayer
              edges={mesh.edges}
              boxes={layout.anchors}
              windowSec={windowSec}
              showLabels={false}
              muted
              undirected
              bow={bowSide}
              selected={null}
              width={layout.width}
              height={layout.height}
              testId="map-neighborhood-mesh"
            />
            <EdgeLayer
              edges={shown}
              boxes={layout.anchors}
              // Container ports are edge endpoints, not obstacles: a label
              // only has to clear the card that owns them.
              avoid={[...layout.cards.values()]}
              // A hop now ends on a container row, which is a few pixels
              // inside the card border. Behind the card that terminal stretch
              // would be covered by the card's translucent fill, so the curves
              // go above it — only the last few pixels ever overlap a card,
              // and the labels keep clearing them either way.
              above
              windowSec={windowSec}
              // Container rows are not nodes: highlighting by id would dim every
              // edge that ends on one, so the panel leaves the whole hop lit.
              selected={null}
              width={layout.width}
              height={layout.height}
              testId="map-neighborhood-edges"
            />
            {layout.slots.map((slot) => {
              const peer = nodes.get(slot.id);
              const box = layout.cards.get(slot.key);
              if (!peer || !box) return null;
              const info = containers.get(slot.key);
              const rows: NeighborContainer[] = (info?.names ?? []).map((name) => ({
                name,
                rate: rate(perRow.get(rowId(slot.key, name)) ?? 0),
                ports: usedPorts.get(rowId(slot.key, name)) ?? {},
              }));
              if (info?.extra) {
                rows.push({ name: `+${info.extra} more containers`, rate: 0, more: true });
              }
              return (
                <NeighborCard
                  key={slot.key}
                  node={peer}
                  box={box}
                  containers={rows}
                  cardPorts={usedCardPorts.get(slot.key)}
                  cpu={peer.cpu_ns ?? 0}
                  cpuMax={cpuMax}
                  selected={slot.side === "self"}
                  relation={
                    slot.side === "up" ? "caller" : slot.side === "down" ? "callee" : undefined
                  }
                  title={slotLabel(peer, slot)}
                  subtitle={slotMeta(peer, slot)}
                  selectId={slot.id}
                  podUid={slot.podUid}
                  onSelect={onSelect}
                  testId={slot.id === node.id ? "map-neighborhood-self" : "map-neighborhood-peer"}
                />
              );
            })}

            {inbound.length === 0 && outbound.length === 0 ? (
              <p className="absolute inset-0 flex items-center justify-center text-[11px] text-muted-foreground">
                {nodeLabel(node)} has no observed peers in this window.
              </p>
            ) : null}
          </div>
          <span className="pointer-events-none absolute bottom-1 left-2 text-[9px] text-muted-foreground/60">
            drag to pan · scroll to zoom
          </span>
        </div>
      ) : null}
      {/* Why a line is missing is a fair question, so the rule is on screen:
          the hops through the selected workload, plus the arcs between two
          cards on the same side. */}
      {mesh.edges.length > 0 ? (
        <p className="mt-1 text-[10px] text-muted-foreground/80" data-testid="map-mesh-note">
          Arcs outside a column are traffic between two callers or two callees
          {mesh.hidden > 0 ? ` (heaviest ${MAX_ARCS} shown, ${mesh.hidden} more)` : ""}; a
          caller and a callee already meet through {nodeLabel(node)}.
        </p>
      ) : null}
      {hiddenIn > 0 || hiddenOut > 0 ? (
        <p className="mt-1 text-[10px] text-muted-foreground">
          {hiddenIn > 0 ? `+${hiddenIn} more callers` : ""}
          {hiddenIn > 0 && hiddenOut > 0 ? " · " : ""}
          {hiddenOut > 0 ? `+${hiddenOut} more callees` : ""}
          {" — the full lists are in the Net tab."}
        </p>
      ) : null}
    </div>
  );
}
