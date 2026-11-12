# Dashboard

The dashboard is embedded in the server binary (`rust-embed`) and served from
the same port as the API — nothing to build, nothing to host separately.

```
http://localhost:4040/
```

This page is the *user* guide: what each control does, how to reproduce a view
in a URL, and what to expect from the rendering. The endpoint shapes are in
[api.md](api.md); the storage and query design are in
[architecture.md](architecture.md).

## Opening a specific view

| Form | Meaning |
| --- | --- |
| `/?tenant=acme` | open as tenant `acme` (also written to `localStorage`) |
| `/#/compare` | open the compare tab |
| `/#/flamegraph?type=off_cpu&range=6h` | last six hours of off-CPU |
| `/#/flamegraph?range=6h&wfrom=…&wuntil=…` | twenty minutes inside the last six hours |
| `/#/flamegraph?selector=pod%3D%22cart-1%22` | one pod, selector URL-encoded |
| `/#/compare?from=…&until=…&bwfrom=…&bwuntil=…` | two absolute windows (ns) |

`range` is a relative range (`5m`, `15m`, `1h`, `6h`, `24h`, `7d`); `from` /
`until` are an absolute range in nanoseconds and take precedence. `wfrom` /
`wuntil` narrow the window inside that range and may be omitted when the window
is the whole range. The full table is under [URL reference](#url-reference).

If the server was started with `--auth-token`, the dashboard asks for a bearer
token once:

```js
localStorage.setItem("lidlesseye.token", "…"); // then reload
```

A tenant-scoped token pins the tenant; the selector is then hidden because
there is nothing to choose.

## Layout

```
┌ top bar ─ tabs · tenant · version · refresh ──────────────────────────┐
├ filter bar ─ profile · group by · range preset · auto · apply ────────┤
├ filter rows ─ [+ Filter]  label = value  … ───────────────────────────┤
├ time bar ─ mode · date pickers · histogram of the chosen range ───────┤
└ main ─────── the selected view ───────────────────────────────────────┘
```

Everything above `main` is shared by all views: changing the window, a filter
or the profile type re-queries whatever view is open.

## Time range

The time selector works in two steps, because the two questions are different:
*which stretch of time am I looking at*, and *which part of it do I want to
analyse*.

1. **Choose the range first** — the segmented control **Relative | Absolute**.
   - *Relative* keeps the preset from the toolbar's **Range** dropdown: `Last
     hour` means the hour before now, and **Auto** (a 30-second reload) makes it
     follow the clock.
   - *Absolute* opens a `From → To` pair of date-time pickers (`Apply`, or
     `Enter` in either field). `Now` moves the end to the present.
2. **Then slide inside it** — the histogram spans exactly the range you chose,
   and the bracket picks the query window inside that range.

| Gesture | Result |
| --- | --- |
| drag inside a narrowed bracket | move it, keeping the span |
| drag the left or right edge | resize that edge |
| drag across the histogram | draw a new window |
| click inside a narrowed bracket | jump there, keeping the span |
| **Full** (top right of a bracket) | back to the whole chosen range |

While the bracket covers the whole range there is nothing to move, so dragging
inside starts a new selection — zoom in, then pan. The minimum span is 1/300 of
the range (at least one second).

Two labels keep this legible: the bracket reads `14:03:10 → 14:43:10 · 40m0.0s`
(what you are looking at) and next to it `of 12:25:00–14:43:10` (the range it
may slide in). The window is what every view queries; the range is what the
picker says.

If the chosen range has no samples, the header says so and offers **Use data
range**, which jumps the picker to the oldest and newest sample the tenant has.
The histogram is decoration on top of the query path: if it cannot load
(expired token, a 5xx), the views keep working.

With **Auto** on and a relative range, the bracket moves with the range, so the
same sub-interval stays selected as time passes. An absolute range is pinned.

## Filtering by label

**+ Filter** adds a row of `[label] [operator] [value]`:

- **label** — every name `/api/v1/label-names` reports in the range, plus
  `service_name`.
- **operator** — `is` (`=`), `is not` (`!=`), `matches` (`=~`), `does not
  match` (`!~`). The two `~` operators are **glob** matches (`*`, `?`), not
  regular expressions, matching the server.
- **value** — the values `/api/v1/label/{name}/values` reports, already
  narrowed by the *other* rows. Pick `node` first and the `pod` list shrinks to
  that node.

In a Kubernetes deployment most of the names are `k8s_*`: `k8s_namespace`,
`k8s_workload`, `k8s_workload_kind`, `k8s_pod` and `k8s_pod_uid` (see
[deployment](deployment.md#naming-series-after-the-pod)). `k8s_workload` is the
one to reach for first — it spans every pod of a Deployment across nodes, which
`k8s_pod` by definition does not.

Rows are ANDed. Together they compose the selector string shown to the right of
the rows, which is exactly what goes on the wire:

```
service_name="checkout",pod=~"checkout-*"
```

The **Labels** view is another way in: click *Filter* next to a value and it
becomes a row (and the flamegraph opens).

A matcher the rows cannot represent — for example `service_name=checkout`
without quotes, which the server also accepts — is shown as a read-only row
rather than dropped, so a pasted deep link keeps filtering the same way.

## Views

### Flamegraph

Rows are stack depths, width is inclusive value; the leaf is at the bottom of
each stack. Clicking a frame zooms into it, `Esc` (or *Reset zoom*) goes back
out, and the breadcrumb above the canvas shows the path. Hovering lights the
hovered frame together with its ancestors. The search box dims everything that
does not match a frame name and reports how many frames matched.

With **Group by** set, the first row under the root is one synthetic frame per
label value (`pod=cart-1`, …), which is the fastest way to see whether one pod
differs from the rest. Very large trees are capped at 50 000 nodes; the legend
says when the result was truncated.

Both the load and every zoom are animated (see [Motion](#motion)). The graph is
a canvas, so text is not selectable and not exposed to screen readers — the
**Top functions** table is the accessible equivalent of the same data.

### Compare

The same selection over two windows in one tree: **A · Baseline** on the left,
**B · Comparison** on the right. Colour marks the delta — red is more in the
comparison window, green is less — and frames present in only one window get a
zero on the other side, which is what makes a regression or a disappearance
visible.

The compare view has **two** histogram brushes, one per window. Both are
independent and absolute. B starts as the current window; A starts as the span
immediately before it. Dragging either one updates the diff; B also becomes the
window the other tabs use.

### Top functions

Sortable by name, self value or total value, with a text filter and a share-of-
total column. `self` counts stacks where the function is the leaf, `total`
counts every stack containing it.

### Time series

Stacked bars, one series per `(service, labels)` — or per label value with
`group_by`. Clicking a legend entry hides that series; hovering a bucket shows
the breakdown. Bucket width follows the window (90 buckets by default), and a
batch contributes proportionally to every bucket it overlaps, so a short batch
does not spike a wide bucket.

### Labels

Label names on the left, values on the right with their totals and a bar for
relative weight. Click *Filter* to turn a value into a filter row.

### Overview

Store statistics (batches, physical and logical bytes, value, oldest/newest),
the profile types present, and the series table with per-series value and time
range. This is the view that answers "is anything arriving at all?".

## URL reference

The hash carries the view and everything that shapes the query:

| Parameter | Example | Meaning |
| --- | --- | --- |
| view | `#/flamegraph` | `flamegraph`, `compare`, `top`, `timeseries`, `labels`, `overview` |
| `type` | `off_cpu` | profile type, including the virtual `wall` |
| `range` | `1h` | relative range: the preset the bracket may slide in |
| `from`, `until` | `1794486354991000000` | absolute range, nanoseconds (replaces `range`) |
| `wfrom`, `wuntil` | `1794482754991000000` | the window inside the range; omitted when it is the whole range |
| `bwfrom`, `bwuntil` | `1794482754991000000` | compare view's baseline window |
| `selector` | `pod="cart-1"` | label selector (URL-encoded) |
| `group_by` | `pod` | split frames / series by this label |
| `tenant` | `acme` | tenant to read |

So `#/flamegraph?range=6h&wfrom=…&wuntil=…` reads "inside the last six hours,
this twenty minutes", and `#/flamegraph?from=…&until=…` reads "this exact
range, all of it". A link with only `from`/`until` therefore still means what it
used to: the range *is* the window.

`?tenant=` in the query string also works, which is the form a shared link
usually takes.

`localStorage` keys: `lidlesseye.tenant` and `lidlesseye.token`.

## Keyboard

| Key | Action |
| --- | --- |
| `r` | reload the current view |
| `Esc` | leave the zoomed frame (flamegraph) |

Shortcuts are ignored while a text field has focus, so typing `r` into the
search box does not reload the page.

## Motion

The flamegraph animates on purpose and sparingly: bars grow in on load, zoom
interpolates the viewport over ~300 ms with an ease-in-out curve, and hover
outlines the ancestry. Each effect is capped under a second, and nothing
animates that would not otherwise be still.

With `prefers-reduced-motion: reduce` the JavaScript renders the final frame
immediately and the CSS transitions collapse, so the dashboard is fully usable
without animation.

## How it is built

React 19 with shadcn/ui over a Next.js static export, and no second process:
`next build` emits `web/out`, `npm run sync` copies it to `web/dist`, and
`rust-embed` inlines that into the binary at compile time. `web/dist` is
committed so `cargo build` still works without Node.

| Path | Responsibility |
| --- | --- |
| `app/`, `components/` | shell, navigation, tables and filter UI (shadcn components) |
| `lib/store.ts` | state, query construction, view switching, URL sync |
| `lib/hash.ts`, `lib/time.ts` | hash <-> state, domain/window arithmetic |
| `lib/selector.ts` | filter rows and selector composition/parsing |
| `lib/canvas/flamegraph.ts` | canvas flamegraph: layout, zoom, hover, animation |
| `lib/canvas/timebrush.ts` | histogram + draggable window |
| `lib/canvas/chart.ts` | stacked time-series canvas |
| `components/views/` | top table, label explorer, overview, empty states |

### Working on the dashboard

```bash
lidlesseye serve --listen 127.0.0.1:4040   # any agent with data
cd web && npm run dev                      # http://127.0.0.1:3000, hot reload
```

The dev server proxies `/api`, `/ingest` and `/metrics` to
`LIDLESSEYE_DEV_SERVER` (default `http://127.0.0.1:4040`): `output: "export"`
and `rewrites` cannot coexist, so development drops the export and `next build`
restores it. Nothing has to be rebuilt to see a change — `make ui` (build +
sync into `web/dist`) is only needed before `cargo build` or a release.

`web/test/smoke.cjs` is the end-to-end check; `scripts/ui-smoke.sh` starts a
server, seeds two tenants and drives the real dashboard in Chromium.
| `format.js` | value/time/byte formatting, escaping |

All state lives in one `state` object in `app.js`. Each view is a class with
`setData(payload)` and `destroy()`; the shell mounts at most one at a time. A
query is a pure function of `state` (`queryParams()` / `diffParams()`), which is
why a URL reproduces a view exactly.

The histograms are filled from `/api/v1/timeseries` over the extent, bucketed
120 ways, and summed across series; the window itself is client state until it
lands in the URL.

### Testing it

```bash
./scripts/ui-smoke.sh        # builds the debug binary, seeds two tenants,
                             # drives Chromium: 46 checks, screenshots in .tmp/ui
./scripts/demo.sh            # throwaway instance on :4040 with 45 minutes of
                             # data in two tenants — the quickest way to see the
                             # histogram do something
```

The smoke test covers the things a unit test cannot: that the canvas paints,
that the brush gesture really changes the window (and that the top table then
shows the older batch and not the newer one), that compare renders two brushes
and dragging one leaves the other alone, that filter rows compose a selector,
and that a non-representable fragment survives a round trip. It fails on any
console error.

`canvas[data-motion]` marks the flamegraph while it animates so the test can
wait for a still frame instead of sleeping.

## Limits and known behaviour

- The histogram spans only the range you chose. A range with no samples shows
  an empty histogram and the **Use data range** hint rather than silently
  widening itself.
- Compare ignores **Group by**; the diff tree is the same shape as a flamegraph
  without synthetic frames.
- Entering compare with the whole range selected splits it in half (first half
  baseline, second half comparison) — a diff of a range against itself is not
  useful. Narrow the bracket first if you want a different split.
- Label filter values are looked up in the chosen range; if it holds no
  samples, the picker falls back to the last seven days and says so in the
  tooltip instead of showing an empty list.
- Auto-refresh is 30 seconds and only reloads. With a relative range it also
  moves the window along, so the same sub-interval stays selected.
- Queries are not cached: every change (filters, window, range, profile type,
  view) hits the API. That is intentional — the data is live and the query path
  is the thing being inspected.
