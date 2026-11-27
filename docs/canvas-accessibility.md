# Canvas accessibility decisions

The flamegraph is drawn into a `<canvas>`, so its frames exist only as pixels.
Two guidance tracks apply: the standard accessibility work and the experimental
HTML-in-Canvas API (`drawable` / `drawElementImage`).

## Adopted (standard, cross-browser)

- The canvas carries `role="img"` and an `aria-label` that states the total and
  points at the text alternative (`web/components/analysis/graph-workspace.tsx`).
- The frame list and the export are the semantic alternatives the accessibility
  guidance asks for on complex charts.

## Not adopted: HTML-in-Canvas (`expose-canvas-content-to-browser-features`)

Evaluated after `set-allow-volatile-features` was enabled (`.mwgrc`), and not
taken for the flamegraph:

- It is a volatile feature shipped in one engine only (Chrome 147 at the time of
  writing) and may still change.
- Adopting it means rebuilding the renderer around `drawable` elements,
  `onpaint`, and `drawElementImage`, i.e. replacing the code path that draws
  every view today, for a benefit the `role="img"` + text-alternative route
  already delivers.
- The feature is worth revisiting when it reaches Baseline and when the graph
  needs per-frame hit testing and screen-reader focus inside the canvas.

Revisit trigger: the API reaching Baseline in at least two engines, or a
request for keyboard navigation between individual frames.
