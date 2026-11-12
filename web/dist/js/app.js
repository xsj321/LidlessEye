// Application shell: filter bar, label filters, histogram time selection,
// hash routing, data loading.
//
// The time window is always resolved to an absolute [from, until] before a
// query goes out: presets are translated to `now-<range>` server-side, the
// histogram brush below the filter bar sets absolute bounds. The compare view
// carries a second, independent window for the baseline side of the diff.

import { ApiError, api, getTenant, setTenant } from "./api.js";
import { TimeSeriesChart } from "./chart.js";
import { Flamegraph } from "./flamegraph.js";
import { LabelExplorer, TopTable, emptyState, renderOverview } from "./views.js";
import { TimeBrush } from "./timebrush.js";
import { LabelFilters, composeSelector, parseSelector } from "./labelfilters.js";

const RANGES = [
  ["5m", "Last 5 minutes"],
  ["15m", "Last 15 minutes"],
  ["1h", "Last hour"],
  ["6h", "Last 6 hours"],
  ["24h", "Last 24 hours"],
  ["7d", "Last 7 days"],
];

const VIEWS = ["flamegraph", "compare", "top", "timeseries", "labels", "overview"];

const state = {
  view: "flamegraph",
  type: "cpu",
  // Either a preset ("1h") or "custom": an absolute window set by dragging
  // the histogram brush.
  range: "1h",
  fromNs: 0,
  untilNs: 0,
  // Baseline window for the compare view; initialised to the preceding span.
  baseFromNs: 0,
  baseUntilNs: 0,
  filters: [],
  groupBy: "",
  tenant: getTenant(),
  auto: false,
  unit: "nanoseconds",
  label: null,
};

const el = {
  main: document.getElementById("main"),
  tabs: document.getElementById("tabs"),
  tenant: document.getElementById("tenant"),
  profileType: document.getElementById("profile-type"),
  groupBy: document.getElementById("group-by"),
  range: document.getElementById("range"),
  filters: document.getElementById("filters"),
  timebar: document.getElementById("timebar"),
  auto: document.getElementById("auto-refresh"),
  apply: document.getElementById("apply"),
  refresh: document.getElementById("refresh"),
  banner: document.getElementById("banner"),
  build: document.getElementById("build-info"),
};

let component = null;
let autoTimer = null;
let requestId = 0;

// The histogram brushes. One in every view, two in compare.
const brushes = { main: null, baseline: null };
let histData = null; // last /api/v1/timeseries payload over the extent
let histToken = 0;
let earliestNs = null; // per tenant, for the brush extent
const valueCache = new Map();

// ---- time windows ------------------------------------------------------

function nowNs() {
  return Date.now() * 1e6;
}

/** `1h` / `30m` / `7d` in nanoseconds. */
function durationNs(text) {
  const match = /^(\d+)([smhd])$/.exec(String(text).trim());
  if (!match) return 3_600_000_000_000;
  const value = Number(match[1]);
  const scale = { s: 1e9, m: 6e10, h: 3.6e12, d: 8.64e13 }[match[2]];
  return value * scale;
}

/** Absolute window in ns, whatever the range preset says. */
function resolveWindow() {
  if (state.range === "custom" && state.fromNs && state.untilNs > state.fromNs) {
    return { from: state.fromNs, until: state.untilNs };
  }
  const until = nowNs();
  return { from: until - durationNs(state.range), until };
}

/** RFC3339, the format the server's time parser accepts. */
function iso(ns) {
  return new Date(Number(ns) / 1e6).toISOString();
}

/** The baseline window: defaults to the span immediately before the main one. */
function resolveBaseline() {
  if (state.baseFromNs && state.baseUntilNs > state.baseFromNs) {
    return { from: state.baseFromNs, until: state.baseUntilNs };
  }
  const win = resolveWindow();
  const span = win.until - win.from;
  return { from: win.from - span, until: win.from };
}

function setWindow(from, until) {
  state.range = "custom";
  state.fromNs = from;
  state.untilNs = until;
  syncControls();
  writeHash();
  load();
  loadHistogram();
}

function setBaselineWindow(from, until) {
  state.baseFromNs = from;
  state.baseUntilNs = until;
  writeHash();
  load();
  loadHistogram();
}

// ---- routing -----------------------------------------------------------

function parseHash() {
  const raw = location.hash.replace(/^#\/?/, "");
  const [view, query] = raw.split("?");
  const params = new URLSearchParams(query ?? "");
  if (VIEWS.includes(view)) state.view = view;
  if (params.get("type")) state.type = params.get("type");
  if (params.get("range")) state.range = params.get("range");
  const from = Number(params.get("from"));
  const until = Number(params.get("until"));
  if (Number.isFinite(from) && Number.isFinite(until) && until > from) {
    state.range = "custom";
    state.fromNs = from;
    state.untilNs = until;
  }
  const bfrom = Number(params.get("bfrom"));
  const buntil = Number(params.get("buntil"));
  if (Number.isFinite(bfrom) && Number.isFinite(buntil) && buntil > bfrom) {
    state.baseFromNs = bfrom;
    state.baseUntilNs = buntil;
  }
  if (params.has("selector")) state.filters = parseSelector(params.get("selector"));
  if (params.has("group_by")) state.groupBy = params.get("group_by");
  if (params.has("tenant")) {
    state.tenant = params.get("tenant");
    setTenant(state.tenant);
  }
}

function writeHash() {
  const params = new URLSearchParams();
  params.set("type", state.type);
  if (state.range === "custom") {
    params.set("from", String(state.fromNs));
    params.set("until", String(state.untilNs));
  } else {
    params.set("range", state.range);
  }
  if (state.view === "compare") {
    const baseline = resolveBaseline();
    params.set("bfrom", String(baseline.from));
    params.set("buntil", String(baseline.until));
  }
  const selector = composeSelector(state.filters);
  if (selector) params.set("selector", selector);
  if (state.groupBy) params.set("group_by", state.groupBy);
  if (state.tenant) params.set("tenant", state.tenant);
  const next = `#/${state.view}?${params.toString()}`;
  if (location.hash !== next) history.replaceState(null, "", next);
}

// ---- data --------------------------------------------------------------

function queryParams() {
  const win = resolveWindow();
  const params = {
    profile_type: state.type,
    from: state.range === "custom" ? iso(win.from) : `now-${state.range}`,
    until: state.range === "custom" ? iso(win.until) : "now",
  };
  const selector = composeSelector(state.filters);
  if (selector) params.selector = selector;
  if (state.groupBy && state.view !== "compare") params.group_by = state.groupBy;
  return params;
}

function diffParams() {
  const params = queryParams();
  const baseline = resolveBaseline();
  params.baseline_from = iso(baseline.from);
  params.baseline_until = iso(baseline.until);
  return params;
}

function setBanner(message) {
  if (!message) {
    el.banner.hidden = true;
    return;
  }
  el.banner.hidden = false;
  el.banner.innerHTML = message;
}

async function load() {
  const id = ++requestId;
  const params = queryParams();
  setBanner("");
  el.main.dataset.loading = "1";

  try {
    if (state.view === "overview") {
      const [stats, types, series] = await Promise.all([
        api.stats(),
        api.profileTypes(params),
        api.series(params),
      ]);
      if (id !== requestId) return;
      renderOverview(el.main, { stats, types: types.profile_types, series: series.series });
      return;
    }

    if (state.view === "flamegraph") {
      const result = await api.flamegraph(params);
      if (id !== requestId) return;
      if (!result.total) {
        renderEmptyFlamegraph();
        return;
      }
      if (!(component instanceof Flamegraph)) {
        mount((box) => new Flamegraph(box));
      }
      component.setData(result);
      return;
    }

    if (state.view === "compare") {
      const result = await api.diff(diffParams());
      if (id !== requestId) return;
      if (!result.total_left && !result.total_right) {
        renderEmpty("Nothing to compare", "Neither window has samples for this profile and filter.");
        return;
      }
      if (!(component instanceof Flamegraph)) {
        mount((box) => new Flamegraph(box));
      }
      component.setData(result);
      return;
    }

    if (state.view === "top") {
      const result = await api.top(params);
      if (id !== requestId) return;
      if (!result.items.length) {
        renderEmpty("Nothing in this range", "No stacks matched the selected profile and filters.");
        return;
      }
      if (!(component instanceof TopTable)) mount((box) => new TopTable(box));
      component.setData(result);
      return;
    }

    if (state.view === "timeseries") {
      const unit = await unitFor(state.type, params);
      const result = await api.timeseries(params);
      if (id !== requestId) return;
      if (!(component instanceof TimeSeriesChart)) mount((box) => new TimeSeriesChart(box));
      component.setData(result, unit);
      return;
    }

    if (state.view === "labels") {
      await loadLabels(id, params);
      return;
    }
  } catch (err) {
    if (id !== requestId) return;
    handleError(err);
  } finally {
    if (id === requestId) delete el.main.dataset.loading;
  }
}

// ---- histogram time bar ------------------------------------------------

/** The full span the brushes show: from the oldest data to now. */
function computeExtent() {
  const win = resolveWindow();
  let from = win.from;
  let until = Math.max(win.until, nowNs());
  if (state.view === "compare") {
    const baseline = resolveBaseline();
    from = Math.min(from, baseline.from);
    until = Math.max(until, baseline.until);
  }
  if (earliestNs) from = Math.min(from, earliestNs);
  const span = until - from;
  return { from: Math.floor(from - span * 0.02), until: Math.ceil(until) };
}

function renderTimebar() {
  brushes.main?.destroy();
  brushes.baseline?.destroy();
  brushes.baseline = null;
  el.timebar.innerHTML = "";

  const extent = computeExtent();
  const win = resolveWindow();

  if (state.view === "compare") {
    const baseline = resolveBaseline();
    const wrap = document.createElement("div");
    wrap.className = "brushes";
    el.timebar.appendChild(wrap);

    // One host per brush: a brush renders by replacing its host's innerHTML,
    // so two brushes in one host would clobber each other.
    const baselineHost = document.createElement("div");
    const mainHost = document.createElement("div");
    wrap.append(baselineHost, mainHost);

    brushes.baseline = new TimeBrush(baselineHost, {
      label: "A · Baseline",
      onChange: setBaselineWindow,
    });
    brushes.main = new TimeBrush(mainHost, {
      label: "B · Comparison",
      onChange: (from, until) => setWindow(from, until),
    });
    brushes.baseline.setExtent(extent.from, extent.until);
    brushes.main.setExtent(extent.from, extent.until);
    brushes.baseline.setSelection(baseline.from, baseline.until);
    brushes.main.setSelection(win.from, win.until);
  } else {
    brushes.main = new TimeBrush(el.timebar, {
      label: "Time range",
      onChange: (from, until) => setWindow(from, until),
    });
    brushes.main.setExtent(extent.from, extent.until);
    brushes.main.setSelection(win.from, win.until);
  }

  if (histData) feedBrushes();
}

function feedBrushes() {
  brushes.main?.setHistogram(histData);
  brushes.baseline?.setHistogram(histData);
}

/** Refresh the histogram over the current extent; single-flighted. */
async function loadHistogram() {
  const token = ++histToken;
  const extent = computeExtent();
  const selector = composeSelector(state.filters);
  const params = {
    profile_type: state.type,
    from: iso(extent.from),
    until: iso(extent.until),
    buckets: 120,
  };
  if (selector) params.selector = selector;
  try {
    const payload = await api.timeseries(params);
    if (token !== histToken) return;
    histData = payload;
    feedBrushes();
  } catch {
    // The histogram is decoration on top of a working query path; a failure
    // here (token expiry, transient 5xx) must not blank the views.
  }
}

async function loadStats() {
  try {
    const stats = await api.stats();
    earliestNs = stats?.store?.earliest_ns || null;
  } catch {
    earliestNs = null;
  }
}

// ---- label filters -----------------------------------------------------

async function loadValuesFor(label) {
  const others = composeSelector(
    state.filters.filter((filter) => !filter.raw && filter.label !== label),
  );
  const key = `${state.tenant}|${label}|${others}`;
  if (valueCache.has(key)) return valueCache.get(key);
  const win = resolveWindow();
  const params = { from: iso(win.from), until: iso(win.until) };
  if (others) params.selector = others;
  const values = api
    .labelValues(label, params)
    .then((result) => (result.values ?? []).map((row) => row.value))
    .catch(() => []);
  valueCache.set(key, values);
  return values;
}

function onFiltersChange(filters) {
  state.filters = filters;
  valueCache.clear();
  writeHash();
  load();
  loadHistogram();
}

async function loadLabels(id, params) {
  const names = await api.labelNames(params);
  if (id !== requestId) return;
  if (!(component instanceof LabelExplorer)) {
    mount(
      (box) =>
        new LabelExplorer(box, {
          onFilter: (label, value) => {
            state.filters = [{ label, op: "=", value }];
            filtersComponent.setFilters(state.filters);
            writeHash();
            setView("flamegraph");
          },
        }),
    );
  }
  const list = names.label_names ?? [];
  component.setNames(list);
  const active = state.label && list.includes(state.label) ? state.label : component.active;
  if (active) {
    component.select(active);
    const values = await api.labelValues(active, params);
    if (id !== requestId) return;
    component.setValues(active, values.values ?? []);
  }
}

// The structured filter rows in the bar under the filterbar.
const filtersComponent = new LabelFilters(el.filters, {
  loadValues: loadValuesFor,
  onChange: onFiltersChange,
});

// ---- misc --------------------------------------------------------------

async function unitFor(type, params) {
  try {
    const types = await api.profileTypes(params);
    const match = (types.profile_types ?? []).find((row) => row.name === type);
    return match?.unit ?? "nanoseconds";
  } catch {
    return "nanoseconds";
  }
}

function handleError(err) {
  if (err instanceof ApiError && err.status === 401) {
    setBanner(
      "This server requires an API token. Run <code>localStorage.setItem('lidlesseye.token', '…')</code> in the console and reload.",
    );
    return;
  }
  if (err instanceof ApiError && err.status === 403) {
    setBanner(
      `${err.message} — switch tenant with the <em>Tenant</em> selector, or clear <code>lidlesseye.tenant</code> in localStorage.`,
    );
    return;
  }
  if (err instanceof ApiError && err.status === 429) {
    setBanner(`${err.message}`);
    return;
  }
  setBanner(`Request failed: ${err.message}`);
}

function mount(factory) {
  component?.destroy?.();
  hideTooltip();
  el.main.innerHTML = "";
  delete el.main.dataset.loading;
  const box = document.createElement("div");
  el.main.appendChild(box);
  component = factory(box);
}

function hideTooltip() {
  const tooltip = document.getElementById("tooltip");
  if (tooltip) tooltip.hidden = true;
}

/** Mirror the URL state into the filter controls. */
function syncControls() {
  el.range.value = state.range;
  el.groupBy.value = state.groupBy;
  if (![...el.profileType.options].some((option) => option.value === state.type)) {
    const option = document.createElement("option");
    option.value = state.type;
    option.textContent = state.type;
    el.profileType.appendChild(option);
  }
  el.profileType.value = state.type;
  el.tenant.hidden = el.tenant.options.length <= 1 && !state.tenant;
  if (state.tenant && ![...el.tenant.options].some((o) => o.value === state.tenant)) {
    const option = document.createElement("option");
    option.value = state.tenant;
    option.textContent = state.tenant;
    el.tenant.appendChild(option);
  }
  el.tenant.value = state.tenant;
  if (el.filters.dataset.selector !== composeSelector(state.filters)) {
    filtersComponent.setFilters(state.filters);
  }
}

function renderEmpty(title, hint) {
  component?.destroy?.();
  el.main.innerHTML = "";
  emptyState(el.main, { title, hint });
  component = null;
}

function renderEmptyFlamegraph() {
  component?.destroy?.();
  el.main.innerHTML = "";
  const tenantFlag = state.tenant ? ` --tenant ${state.tenant}` : "";
  const command = `lidlesseye collect --server http://${location.host}${tenantFlag} --frequency 99`;
  emptyState(el.main, {
    title: "No samples yet",
    hint: state.tenant
      ? `Tenant "${state.tenant}" has no data in this range. Start an agent pointed at it, or switch tenant.`
      : "Start an agent on a machine with eBPF enabled, or point an existing one here.",
    command,
  });
  component = null;
}

// ---- UI wiring ---------------------------------------------------------

function populateSelects() {
  el.range.innerHTML =
    RANGES.map(([value, label]) => `<option value="${value}">${label}</option>`).join("") +
    '<option value="custom">Custom (drag the histogram)</option>';
  el.profileType.innerHTML = '<option value="cpu">cpu</option>';
}

async function loadTenants() {
  try {
    const result = await api.tenants();
    const list = (result.tenants ?? []).map((row) => row.id);
    if (!list.length) list.push("default");
    if (state.tenant && !list.includes(state.tenant)) list.push(state.tenant);
    el.tenant.innerHTML = list
      .map((id) => `<option value="${id}">${id}</option>`)
      .join("");
    if (!state.tenant) state.tenant = list[0] ?? "default";
    // The request layer reads the tenant from storage, so choosing one here has
    // to be persisted too. Without this the first query went to the server's
    // default tenant while the selector already displayed another — the empty
    // state then blamed the wrong tenant, and worse, a populated default
    // tenant would have been shown under someone else's name.
    setTenant(state.tenant);
    el.tenant.value = state.tenant;
    el.tenant.hidden = list.length <= 1;
  } catch (err) {
    // A tenant-scoped token cannot enumerate tenants; that is fine.
    if (!(err instanceof ApiError && (err.status === 403 || err.status === 401))) {
      handleError(err);
    }
    el.tenant.innerHTML = `<option value="${state.tenant || "default"}">${
      state.tenant || "default"
    }</option>`;
    el.tenant.hidden = true;
  }
}

async function loadProfileTypes() {
  try {
    const win = resolveWindow();
    const params = {
      from: state.range === "custom" ? iso(win.from) : `now-${state.range}`,
      until: state.range === "custom" ? iso(win.until) : "now",
    };
    const result = await api.profileTypes(params);
    const types = (result.profile_types ?? []).map((row) => row.name);
    if (!types.includes("cpu")) types.unshift("cpu");
    el.profileType.innerHTML = types
      .map((name) => `<option value="${name}">${name}</option>`)
      .join("");
    if (!types.includes(state.type)) state.type = types[0] ?? "cpu";
    el.profileType.value = state.type;
  } catch (err) {
    handleError(err);
  }
}

async function loadLabelNames() {
  try {
    const result = await api.labelNames({ from: "now-7d", until: "now" });
    const names = (result.label_names ?? []).filter((name) => name !== "service_name");
    el.groupBy.innerHTML =
      '<option value="">none</option>' +
      ["service_name", ...names]
        .map((name) => `<option value="${name}">${name}</option>`)
        .join("");
    el.groupBy.value = state.groupBy;
    filtersComponent.setNames(["service_name", ...names]);
  } catch {
    /* grouping is optional */
  }
}

function applyFromControls() {
  state.type = el.profileType.value || "cpu";
  if (el.range.value !== state.range) {
    state.range = el.range.value;
    if (state.range === "custom" && !state.fromNs) {
      // First switch to custom without a dragged window: pin the current one.
      const win = resolveWindow();
      state.fromNs = win.from;
      state.untilNs = win.until;
    }
    if (state.range !== "custom") {
      state.fromNs = 0;
      state.untilNs = 0;
    }
    renderTimebar();
  }
  state.groupBy = el.groupBy.value;
  writeHash();
  load();
  loadHistogram();
}

function switchTenant(tenant) {
  state.tenant = tenant;
  setTenant(tenant);
  state.label = null;
  valueCache.clear();
  earliestNs = null;
  histData = null;
  writeHash();
  // Profile types and label names are per tenant, so reload them too.
  loadStats().then(() => {
    renderTimebar();
    loadHistogram();
  });
  loadProfileTypes().then(() => {
    loadLabelNames();
    renderTimebar();
    load();
    loadHistogram();
  });
}

function setView(view) {
  state.view = view;
  state.label = null;
  for (const link of el.tabs.querySelectorAll("a")) {
    link.classList.toggle("active", link.dataset.view === view);
  }
  component?.destroy?.();
  component = null;
  hideTooltip();
  el.main.innerHTML = "";
  renderTimebar();
  syncControls();
  writeHash();
  load();
  loadHistogram();
}

function setupAutoRefresh() {
  state.auto = el.auto.checked;
  clearInterval(autoTimer);
  if (state.auto) {
    autoTimer = setInterval(() => {
      // A relative range tracks "now"; a custom window is pinned in place.
      if (state.range !== "custom") {
        const win = resolveWindow();
        brushes.main?.setSelection(win.from, win.until);
        if (state.view === "compare") {
          const baseline = resolveBaseline();
          brushes.baseline?.setSelection(baseline.from, baseline.until);
        }
      }
      load();
      loadHistogram();
    }, 30_000);
  }
}

function boot() {
  populateSelects();
  parseHash();
  syncControls();
  renderTimebar();
  el.auto.checked = state.auto;
  for (const link of el.tabs.querySelectorAll("a")) {
    link.classList.toggle("active", link.dataset.view === state.view);
  }

  el.tabs.addEventListener("click", (event) => {
    const link = event.target.closest("a[data-view]");
    if (!link) return;
    event.preventDefault();
    setView(link.dataset.view);
  });
  window.addEventListener("hashchange", () => {
    parseHash();
    setView(state.view);
  });

  el.apply.addEventListener("click", applyFromControls);
  el.refresh.addEventListener("click", () => {
    load();
    loadHistogram();
  });
  el.auto.addEventListener("change", setupAutoRefresh);
  el.range.addEventListener("change", applyFromControls);
  el.profileType.addEventListener("change", () => {
    state.type = el.profileType.value || "cpu";
    writeHash();
    load();
    loadHistogram();
  });
  el.groupBy.addEventListener("change", applyFromControls);
  el.tenant.addEventListener("change", () => switchTenant(el.tenant.value));
  window.addEventListener("keydown", (event) => {
    if (event.target.matches("input, select, textarea")) return;
    if (event.key === "r" || event.key === "R") {
      load();
      loadHistogram();
    }
  });

  api
    .version()
    .then((info) => {
      el.build.textContent = `v${info.version}`;
    })
    .catch(() => {
      el.build.textContent = "offline";
    });

  loadTenants().then(() => {
    // The extent needs the oldest retained sample, so stats come first.
    loadStats().then(() => {
      renderTimebar();
      loadHistogram();
    });
    loadProfileTypes().then(() => {
      el.profileType.value = state.type;
      loadLabelNames();
      load();
      loadHistogram();
    });
  });
}

// Exposed for debugging from the browser console and for the UI smoke tests.
window.lidlesseye = {
  state,
  view: () => component,
  reload: load,
  setView,
  switchTenant,
};

boot();
