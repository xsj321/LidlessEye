"use client";

import { useCallback } from "react";
import type { ReactNode } from "react";

import { GraphWorkspace } from "@/components/analysis/graph-workspace";
import { Flamegraph } from "@/lib/canvas/flamegraph";
import type { RendererFactory } from "@/lib/analysis/renderer";
import type { DiffPayload, FlamegraphPayload } from "@/lib/types";

/**
 * The canvas flamegraph, plugged into the shared analysis shell.
 *
 * The factory has to keep a stable identity: the shell builds the renderer
 * once, and a new function on every render would tear down the canvas (and its
 * zoom, selection and hover) on every keystroke in the search box.
 */
const createFlamegraph: RendererFactory = (canvas, scroll, callbacks) =>
  new Flamegraph(canvas, scroll, callbacks);

export function FlamegraphCanvas({
  payload,
  fileStem = "lidlesseye-flamegraph",
  header,
}: {
  payload: FlamegraphPayload | DiffPayload;
  /** Base name for the SVG/JSON exports. */
  fileStem?: string;
  /** Strip above the toolbar, e.g. the compare A/B summary. */
  header?: ReactNode;
}) {
  const stable = useCallback(createFlamegraph, []);
  return (
    <GraphWorkspace
      payload={payload}
      createRenderer={stable}
      fileStem={fileStem}
      header={header}
    />
  );
}
