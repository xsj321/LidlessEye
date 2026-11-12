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
| `/#/flamegraph?selector=pod%3D%22cart-1%22` | one pod, selector URL-encoded |
| `/#/compare?from=…&until=…&bfrom=…&buntil=…` | two absolute windows (ns) |

`range` is a relative preset (`5m`, `15m`, `1h`, `6h`, `24h`, `7d`); `from` /
`until` are absolute nanoseconds and take precedence. The full table is under
[URL reference](#url-reference).

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
├ time bar ───── histogram of sample density, draggable window ────────┤
└ main ─────── the selected view ───────────────────────────────────────┘
```

Everything above `main` is shared by all views: changing the window, a filter
or the profile type re-queries whatever view is open.

## Time range

The **Range** preset is relative: `Last hour` means *the hour before now*, so
with **Auto** ticked (a 30-second reload) the view keeps tracking the present.

The **histogram** below the filter bar shows sample density over the tenant's
whole retained extent — oldest sample to now. Its window is the query range.
Once you drag it, the range becomes absolute: the *Custom* entry appears in the
preset list, the exact bounds go into the URL, and auto-refresh reloads that
same window instead of following the clock.

| Gesture | Result |
| --- | --- |
| drag the middle of the window | move it, keeping the span |
| drag the left or right edge | resize that edge |
| drag across empty histogram | draw a new window |
| click outside the window | jump there, keeping the span |

The minimum span is 1/300 of the extent (at least one second). The label above
the canvas reads `14:03:10 → 14:43:10 · 40m0.0s` so the window is legible
without hovering.

The histogram is decoration on top of the query path: if it cannot load
(expired token, a 5xx), the views keep working.

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
| `range` | `1h` | relative preset (mutually exclusive with `from`/`until`) |
| `from`, `until` | `1794486354991000000` | absolute window, nanoseconds |
| `bfrom`, `buntil` | `1794482754991000000` | compare view's baseline window |
| `selector` | `pod="cart-1"` | label selector (URL-encoded) |
| `group_by` | `pod` | split frames / series by this label |
| `tenant` | `acme` | tenant to read |

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

Plain ES modules, no framework and no build step — `rust-embed` inlines
`web/dist` into the binary at compile time, so `cargo build` is the build:

| File | Responsibility |
| --- | --- |
| `index.html`, `styles.css` | shell and theme (CSS custom properties on `:root`) |
| `app.js` | state, hash routing, query construction, view switching |
| `api.js` | fetch wrapper, tenant/token headers, error mapping |
| `flamegraph.js` | canvas flamegraph: layout, zoom, hover, animation |
| `timebrush.js` | histogram + draggable window |
| `labelfilters.js` | filter rows and selector composition/parsing |
| `chart.js` | stacked time-series canvas |
| `views.js` | top table, label explorer, overview, empty states |
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

- The histogram spans the tenant's retained data, so a five-minute selection
  inside a seven-day retention is a thin sliver — the handles still work, but
  the default presets are the easier way to get there.
- Compare ignores **Group by**; the diff tree is the same shape as a flamegraph
  without synthetic frames.
- Auto-refresh is 30 seconds and only reloads; it never moves a custom window.
- Queries are not cached: every change (filters, window, profile type, view)
  hits the API. That is intentional — the data is live and the query path is
  the thing being inspected.
