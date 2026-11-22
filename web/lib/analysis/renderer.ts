// The contract between the analysis workspace and whatever draws a profile.
//
// The workspace owns everything around the graph: the toolbar, breadcrumb,
// zoom controls, export, the details panel and the tooltip. A renderer only
// answers "what is under this point", "draw yourself", "move the viewport".
// That split is what lets a second profile type reuse the shell without
// re-implementing the parts that are actually about analysis (where am I, what
// is selected, how do I get the number out) rather than about drawing.
//
// The flamegraph is the first implementation; a table or a tree view can
// satisfy the same interface.

import type { Crumb, FlameHover, FlameNodeDetail, LegendEntry } from "@/lib/canvas/flamegraph";
import type { DiffPayload, FlamegraphPayload } from "@/lib/types";

export interface GraphCallbacks {
  onCrumbs?: (crumbs: Crumb[]) => void;
  /** The "n of m frames match" line, empty when the search is off. */
  onMatch?: (text: string) => void;
  /** Fraction of the whole graph currently visible, 0..1. */
  onZoom?: (fraction: number) => void;
  onHover?: (hover: FlameHover | null, clientX: number, clientY: number) => void;
  onLegend?: (entries: LegendEntry[]) => void;
  onSelectNode?: (detail: FlameNodeDetail | null) => void;
  /** `C` was pressed; the workspace owns the clipboard. */
  onCopyName?: (name: string) => void;
}

export interface GraphRenderer {
  setData(data: FlamegraphPayload | DiffPayload): void;
  setSearch(text: string): void;
  /** Back to the whole graph. */
  reset(): void;
  /** One step in: the selection if there is one, else the viewport centre. */
  zoomIn(): void;
  /** One step out: up the breadcrumb, then the viewport. */
  zoomOut(): void;
  select(index: number): void;
  clearSelection(): void;
  zoomTo(index: number): void;
  focusCrumb(position: number): void;
  /** The visible graph, as a standalone document. */
  toSvg(title?: string): string;
  toJson(): string;
  destroy(): void;
}

export type RendererFactory = (
  canvas: HTMLCanvasElement,
  scroll: HTMLElement,
  callbacks: GraphCallbacks,
) => GraphRenderer;
