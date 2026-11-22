# Dashboard

The dashboard is embedded in the server binary (`rust-embed`) and served from
the same port as the API — nothing to build, nothing to host separately.

```
http://localhost:4040/
```

A bare URL opens the **service map** — the system's shape before any one
function is chosen. The map's left drawer switches to the other views; see
[USAGE.md](USAGE.md) for a step-by-step walkthrough in English and
[USAGE.zh-CN.md](USAGE.zh-CN.md) in Chinese.

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
┌ drawer ┬ top bar ───────────── tenant · version · refresh ───────────┐
│        ├ toolbar ─ profile · group by · range · auto (not on map) ───┤
│  nav   ├ filter rows ─ [+ Filter]  label = value  … ──────────────────┤
│        ├ time bar ─ mode · relative range · histogram ───────────────┤
│Settings├ main ─────── the selected view ─────────────────────────────┤
└────────┴──────────────────────────────────────────────────────────────┘
```

The **drawer** is a `<nav>` landmark whose buttons carry `aria-current="page"`:
these are pages, not sibling tabs. It collapses to icons; the choice is kept in
localStorage. The collapse button sits beside the wordmark when expanded; once
collapsed it disappears and the expand button becomes the last option of the
icon rail. **Settings** sits at its foot (see [Settings](#settings)).

The top bar intentionally has no title — the brand is already in the drawer.
The tenant selector is on its right, immediately before the build version. The
profile toolbar is omitted entirely on the service map: profile type, grouping,
auto-refresh and Apply all describe a profile query, and the map's relative
range is picked in the time bar next to the histogram.

Everything to the right of `main`'s siblings is shared by all views: changing
the window, a filter or the profile type re-queries whatever view is open.

## Time range

The time selector works in two steps, because the two questions are different:
*which stretch of time am I looking at*, and *which part of it do I want to
analyse*.

1. **Choose the range first** — the segmented control **Relative | Absolute**.
   - *Relative* keeps the preset from the **Range** dropdown on the right of the
     time bar (profile views also have one in the toolbar): `Last hour` means
     the hour before now, and **Auto** (a 30-second reload) makes it follow the
     clock.
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
| hover the histogram | a guide line plus the time and sample count of that bucket |
| **−** / **+** (top right of a bracket) | halve or double the window around its centre |
| **Full** (top right of a bracket) | back to the whole chosen range |
| **Hide chart** / **Show chart** | collapse or reopen the histogram row |

Collapsing leaves the range controls visible and gives the main view the space
back. The brushes stay mounted with their selection and data; the choice is
kept in localStorage and does not appear in the URL. Compare mode keeps both
brushes behind the same toggle.

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
each stack. Hovering lights the hovered frame together with its ancestors and
reports its total, self, samples and depth. The search box dims everything that
does not match a frame name and reports how many frames matched.

Reading and acting are separate. A **click selects** the frame and opens the
details panel beside the canvas; it does not move the graph, so the panel stays
put while its call stack is walked. **Double click zooms** into the frame, and
`Esc` drops the selection before it zooms out. The panel carries the frame's
self and total value, its own **sample count**, its depth, the call stack (each
ancestor is a button) and *Zoom in* / *Copy name*.

The breadcrumb always names the level — *All frames* at the root — so "how deep
am I" has an answer before the first zoom. The toolbar's −/+ buttons zoom
one step around the centre (or into the selection), *Reset zoom* returns to the
whole graph, and **Export** writes what is on screen as SVG or the payload plus
the current zoom as JSON.

The same shell — search, zoom, breadcrumb, details, export — hosts any renderer
that implements `lib/analysis/renderer`; the flamegraph is the first. The time
range stays outside it, in the bar above the views: it is one control for the
whole page, and every view queries the same window.

With **Group by** set, the first row under the root is one synthetic frame per
label value (`pod=cart-1`, …), which is the fastest way to see whether one pod
differs from the rest. Very large trees are capped at 50 000 nodes by default;
the cap is configurable in [Settings](#settings), and the legend says when the
result was truncated.

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

Above the graph, a strip names both windows with their clock times, lengths and
totals, followed by the delta. **Swap** exchanges the two in one click, which is
what "read this diff backwards" needs and what two brushes cannot do without
being dragged one by one.

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

### Service map

The network graph of the selected window: nodes are workloads, ClusterIP
Services and the outside world; edges are TCP connections between them. A node
is a real `Card` — a small rounded rectangle, not a dot — with two lines inside
it: the service name, then its namespace and, for a workload, how many replicas
are ready. Three signals share the card without competing:

- **colour** — a bar on the left edge, and a faint tint, say what the node is:
  blue for a workload, purple for a ClusterIP, grey for the outside world;
- **shape** — a glyph on the right says how it is doing: a filled circle when
  every replica is ready, a triangle when one is not, a hollow ring when the
  cluster reported no replicas at all;
- **a hairline at the bottom** — the share of on-CPU time of the busiest node on
  the map, so cost is a bar rather than a fourth colour.

Every edge is labelled with the application protocol the payload sniffer saw
(or the port's fallback), new connections per second and the mean round-trip
time of the connections that closed in the window (`http · 0.31/s · 149µs`).
Latency picks the unit that keeps a digit before the decimal point — `8µs`,
`149µs`, `12ms`, `1.4s` — instead of printing `0.005s`, and a rate below one per
second switches to per-minute. Labels are *placed*, not dropped at the
geometric midpoint —
each is tried at nine points along its curve and seven offsets from each, and
takes the first spot that clears every card by 4px and every other label by 2px.
When space runs out the label drops the latency first, then the rate, keeping
the protocol as long as it can; only when none of them fit does the edge keep
its curve and lose its text. Overlapping digits are not information,
so nothing is drawn on top of anything else: paths go in one pass and labels in
a second, or the edge drawn last would strike a line through the number.
Retransmits turn an edge red and dashed; an edge that runs against the column
flow (a callback, a retry) bends out and arrives on the far side.

The graph is laid out in columns, one per dependency depth: an entry point sits
in the leftmost column and everything it calls sits to its right. Cycles are
condensed first, so a pair of services that call each other share a column
instead of stacking up. Within a column, cards are ordered to reduce crossings
against the column before them, and the busiest node is highest. Cards in one
column share a width, which is what makes a column read as a stack. Scroll to
zoom, drag the background to pan, double the button group at the top right to
fit the graph back.

Clicking a card slides a panel in from the right — the map stays visible behind
it, because the numbers are about the topology that is still on screen. The
panel opens with the node's **neighbourhood**: callers in a column on the left,
the node in the middle, callees on the right, every edge labelled like the map's
and every card clickable, so walking the graph is one hop per click. A column
with nothing in it is dropped and its space given to the others.

The hop uses its own layout and its own card, not the big map's. A workload is
**one card that owns its containers**: the header names the workload, and below
it one translucent pane per container rests on the card — no spine, no border
and no connector hairline, because the lift and the gaps already say the panes
belong to the card above them. The rows are a real `<ul>` of `<li>` children
and the card is an
`<article>` whose whole area is one native `<button>`, so "workload owns
container" is expressed in the markup, not just in the pixels; Enter/Space walk
the graph without a hand-rolled key handler, and selection only recolours the
border rather than resizing it. A long workload name or a third sidecar cannot
push one over another — the browser stacks them, and card height is computed
from the same shared geometry the card renders with. Containers that carried
traffic in the window come first, heaviest at the top; a long tail collapses
into `+n more containers`.

**Peers line up with the container they talk to.** The layout does not centre
each column independently, which is what made a hop look ragged: a peer card is
placed so that the container row carrying the edge sits at the same height as
the row it connects to on the selected card. The hop therefore reads as a
straight horizontal line between two container ports. Peers that target the
same row are pushed apart by the minimum card gap, so they never touch. A
`CALLERS` / `CALLEES` caption sits above each side, so the direction of traffic
is legible without a legend, and a side with no peers is dropped entirely
rather than leaving an empty column.

**Edges run container to container.** Each container row draws a small port on
its **own left and right edge** — `out` beside the container on the right, `in`
on the left of a callee, both on the selected workload's rows. The endpoint is
the row, not the workload card: the container is what holds a connection, so a
hop has to start and end on the pane that represents it. Because the row is
inset a few pixels inside the card, the edge layer for this drawing is rendered
*above* the cards — behind them, the terminal stretch would be swallowed by the
card's translucent fill and the line would look like it stopped at the card
border after all. Ports are strict: a row that only receives draws no `out`
port, so a callee's dead end never looks like it dials out, and a row with a
port on one side takes that much extra text padding so the dot never covers the
rate. Traffic the agent could not attribute to any container falls back to a
port on the workload's **header edge**, where the name is, instead of the middle
of the body where it would strike through the rows.
Labels clear the workload cards, not the rows inside them, and stay in the
channel between columns. The hop has its own scroll-to-zoom, drag-to-pan and
zoom buttons, with a *fit* action that returns to a readable whole-hop view.

**Two cards on the same side that talk to each other get an arc**, drawn
outside their column. Without it a set of callers that exchange traffic looked
unrelated. Two rules keep it honest: a caller and a callee are *not* joined by
an arc — both already connect to the selected workload, and a curve through the
middle card would read as "both talk to this workload", which is a different
claim; and the arc is undirected and unlabelled, because it answers "do these
two talk", not "how fast". The note under the map states the rule, and a
workload that appears on both sides gets **two cards**, one per direction,
because "they call you" and "you call them" carry different container rows.

Dragging pans from anywhere on the surface, cards included, and the surface
selects no text: a pan that selected labels turned the next drag into a native
text drag, which the browser offers as a search. The pointer is captured only
after the pointer has moved, so a click on a card still opens that card.

The panel is portalled to `document.body` and positioned `fixed`, so it spans
the **full window height** rather than stopping at the map card. It opens at
the width the neighbourhood needs to show that hop at scale 1, clamped to the
window; resizing the graph or clicking another node recomputes it. Drag its
left edge (the `map-panel-resize` separator) to take manual control of the
width; arrow keys nudge by 24 px, Shift+arrow by 64 px, and Home returns to the
map's fitted width. A dragged width is kept in
`localStorage["lidlesseye.map.panel.width"]`.

Below the neighbourhood are seven tabs:

| tab | what it answers | where the numbers come from |
| --- | --- | --- |
| SLO | how much of the traffic failed, against a 99.9% objective | retransmits as the error signal (no L7 status codes are collected) |
| Instances | which replicas exist, whether they are ready, where they run | the cluster's pod list, per bucket |
| CPU | how much on-CPU time, in cores, over the window | the `cpu` profiles behind the node's labels |
| Memory | the same shape for a heap profile type | whatever memory type the emitters ship, if any |
| Net | bytes per second in and out, RTT, per-peer rates | the node's counters and its edges |
| DNS | lookup latency per nameserver | edges on TCP/53 |
| Profiling | the hottest functions, the node's own flamegraph, and the jump to the full view | `/api/v1/top` and `/api/v1/flamegraph` for the same selector |

Charts follow what the data means rather than one house style: a line with an
area fill for anything that moves over time, a step line where the value is a
gauge (replicas), a sorted bar for a ranking, and an error rate — not an
availability percentage — where the interesting variation is in the last
fraction of a percent. When a tab has no data it says which measurement is
missing and what would produce it, instead of drawing an empty chart.

The **Profiling** tab embeds the real `FlamegraphCanvas` below the ranking: it
runs `/api/v1/flamegraph` with the node's selector and the same window, so
search, click-to-zoom, breadcrumbs and the hover tooltip behave exactly as they
do in the full view. An external node has no labels to match, so the tab says
so rather than drawing every stack in the cluster under that name. *Open full
flamegraph view* replaces the filter bar with the same selector and switches
pages. The time range is the one chosen above: dragging the bracket re-queries
the map exactly as it re-queries a flamegraph.

Edge labels appear once the map is zoomed past a threshold (62 % by default),
both to keep the initial whole-graph view readable and because the pills do not
scale with the canvas transform. Both the switch and the threshold are in
[Settings](#settings).

The protocol on an edge comes from the server port; the map does not parse
payloads, so the rate is *new connections* per second, not requests. *External*
collects everything the map cannot name — traffic that really does come from
outside the cluster — so an edge to it is worth a second look. See
[architecture.md](architecture.md#service-map) for what the agent reads and why.

## Settings

Opened from the foot of the drawer. Preferences are kept in
`localStorage["lidlesseye.settings"]` and deliberately stay **out of the URL**:
a link describes a screen, settings describe the tool. Only a URL without an
explicit view or range picks up the start-up values.

| Setting | Default | What it changes |
| --- | --- | --- |
| Language | `en` | `en` / `zh`; switches every interface string in place and is stored in the same preference object. |
| Default view | Service map | Landing view for a bare URL. |
| Default range | `1h` | Relative range on a fresh load. |
| Auto-refresh interval | 30 s | 10 s / 30 s / 1 min / 5 min; re-arms an active Auto timer immediately. |
| Edge labels | on | The rate/RTT pills on service-map edges. |
| Label zoom threshold | 62 % | Zoom level at which those pills appear. |
| Flamegraph frame cap | 50 000 | `max_nodes` on `/flamegraph` and `/diff`, including the panel's embedded graph. |
| Histogram buckets | 120 | 60 / 120 / 240 buckets in the time brush. |

`Reset to defaults` restores every field. Invalid stored values (a deleted view,
an out-of-range number) fall back to the default rather than breaking startup.

## URL reference

The hash carries the view and everything that shapes the query:

| Parameter | Example | Meaning |
| --- | --- | --- |
| view | `#/flamegraph` | `flamegraph`, `compare`, `top`, `timeseries`, `labels`, `servicemap`, `overview` |
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
| `c` | copy the selected (or hovered) frame name |
| `+` / `-` | zoom in / out one step |
| `0` | reset the flamegraph zoom |
| `Esc` | drop the frame selection, then leave the zoomed frame |

Shortcuts are ignored while a text field has focus, so typing `r` into the
search box does not reload the page.

The view switcher is a `<nav>` of buttons with `aria-current="page"`, not a
`tablist`: it switches the whole page and has no sibling panels for the tab
pattern to describe. Its buttons keep `data-view` and `data-state` so the URL
model and the smoke test read the same source of truth.

The service map's cards are disclosures — `aria-expanded` plus
`aria-controls="map-panel"` — and the drawer is a non-modal `role="dialog"`
named by the node title. Non-modal on purpose: the map behind it stays live so
the next card can be clicked without dismissing the current one. Opening moves
focus into the drawer, closing returns it to the card that opened it, and a
closed drawer is `inert`, so its off-screen buttons are not silent tab stops.
The page declares `color-scheme: dark`, which keeps browser-drawn surfaces —
scrollbars, the date/time pickers behind the range inputs, select popups — in
the same palette as the dashboard instead of styling each one by hand.

## Visual rules

The dashboard follows Google Chrome's [Modern Web Guidance](https://github.com/GoogleChrome/modern-web-guidance-src).
It is a working checklist rather than a mood board, and the rules below are the
ones this code has already been measured against:

- A card is an `<article>`. A card with a title and a subtitle groups them in
  `<header>` + `<hgroup>`; repeated items inside a card are a `<ul>`/`<li>`.
- When the whole card is one action, the action is a single native `<button>`
  stretched over the card (`absolute inset-0`) and the focus ring is drawn on
  the card with `has-[>button:focus-visible]`. No `div role="button"` and no
  hand-written Enter/Space handlers — the browser already implements both.
- Selection changes colour only (border-color, box-shadow). Border width,
  padding and font weight never change, so a card does not jump when it is
  selected.
- A card draws one frame, not two. The card's own border and its tint already
  group the header and the rows inside it, so there is no rule under the
  header, no border on a container row, and never two hairlines a pixel apart
  for one boundary.
- Nesting is carried by depth, not linework. A layer inside a card is a
  translucent pane on a translucent card: lighter surface, cast shadow, and
  visible gap. No spine, no connector hairline — the gap and the value
  difference already say which layer the child belongs to.
- Colour carries one meaning per palette. Flamegraph frames are cool hues
  keyed by function name, so the same function keeps its colour between views;
  warm red/green are reserved for the compare view's delta and the accent is
  reserved for selection and hover, so a warm frame never reads as a finding.
- The shell has one `<main>` (focusable, `tabIndex={-1}`), a skip link ahead of
  the navigation drawer, and exactly one `<h1>` naming the current view.

Read a guide before changing a component: `npx -y modern-web-guidance@latest
retrieve card` (or `list` for the full set).

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
`next build` emits `web/out`, `npm run sync` copies it to `web/dist`,
`lidlesseye-server`'s `build.rs` mirrors that into the crate's `ui/`, and
`rust-embed` inlines it into the binary at compile time. The export is not
committed — `make ui` produces it — but `ui/` is what `cargo package` ships, so
the published crate embeds the dashboard too. Without an export, `build.rs`
keeps an empty placeholder so a checkout without Node still compiles (the server
then answers 503 at `/` until the real build is in place).

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

Node 20.19+ is required: the PostCSS pipeline loads ESM-only packages and older
Node cannot `require()` them. `make build`/`release`/`test` run `make ui` first,
which is why a dashboard change is picked up by the next Rust build without an
extra step; `make ui` on its own only rebuilds the export.

The dev server proxies `/api`, `/ingest` and `/metrics` to
`LIDLESSEYE_DEV_SERVER` (default `http://127.0.0.1:4040`): `output: "export"`
and `rewrites` cannot coexist, so development drops the export and `next build`
restores it. Nothing has to be rebuilt to see a change — `make ui` (build +
sync into `web/dist`) is only needed before `cargo build` or a release.

`web/test/smoke.cjs` is the end-to-end check; `scripts/ui-smoke.sh` starts a
server, seeds two tenants and drives the real dashboard in Chromium.
State lives in one store (`lib/store.ts`) read through `useSyncExternalStore`.
`queryParams()` / `diffParams()` derive the request from it as pure functions,
which is why a URL reproduces a view exactly; the shell swaps views by changing
one field, and each view is a component that draws from the store's payload.

Profile histograms are filled from `/api/v1/timeseries` over the extent,
bucketed 120 ways, and summed across series. The service map asks
`/api/v1/servicemap/histogram` instead: it sums `net_edges.connections` per
bucket over the whole map, so the brush shows request density rather than CPU
samples. Both are converted to the same `TimeseriesPayload` envelope before
reaching `TimeBrush`; the window itself is client state until it lands in the
URL.

### Testing it

```bash
./scripts/ui-smoke.sh        # builds the dashboard if needed, then the debug
                             # binary, seeds two tenants and drives Chromium:
                             # 51 checks, screenshots in .tmp/ui
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
