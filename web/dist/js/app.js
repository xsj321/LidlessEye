// Application shell: filter bar, label filters, time range picker + histogram,
// hash routing, data loading.
//
// The time state has two levels on purpose:
//
//   domain — the range you *chose* first (a relative preset, or an absolute
//            from/to). The histogram spans exactly this, so the bracket can
//            only slide inside it.
//   window — the bracket inside the domain; this is what queries use.
//
// Changing the domain resets the window to the whole domain. Dragging the
// bracket narrows or moves the window without touching the domain. The compare
// view keeps two windows (baseline and comparison) in the same domain.

import { ApiError, api, getTenant, setTenant } from "./api.js";
import { TimeSeriesChart } from "./chart.js";
import { Flamegraph } from "./flamegraph.js";
import { formatClock } from "./format.js";
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
  // "relative" (preset ending now) or "absolute" (domainFrom/domainUntil).
  mode: "relative",
  range: "1h",
  domainFrom: 0,
  domainUntil: 0,
  // The bracket inside the domain; 0 means "the whole domain".
  fromNs: 0,
  untilNs: 0,
  // Baseline window for the compare view.
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
  brushes: document.getElementById("brushes"),
  rangeMode: document.getElementById("range-mode"),
  rangeContext: document.getElementById("range-context"),
  absRange: document.getElementById("abs-range"),
  absFrom: document.getElementById("abs-from"),
  absUntil: document.getElementById("abs-until"),
  absApply: document.getElementById("abs-apply"),
  absNow: document.getElementById("abs-now"),
  hint: document.getElementById("range-hint"),
  hintUse: document.getElementById("range-hint-use"),
  auto: document.getElementById("auto-refresh"),
  apply: document.getElementById("apply"),
  refresh: document.getElementById("refresh"),
  banner: document.getElementById("banner"),
  build: document.getElementById("build-info"),
};

let component = null;
let autoTimer = null;
let requestId = 0;

const brushes = { main: null, baseline: null };
let histData = null;
let histToken = 0;
let histEmpty = false;
let lastDomain = null;
// Oldest/newest retained sample, used only for the "no samples here" escape.
let dataRange = null;
const valueCache = new Map();

// ---- time --------------------------------------------------------------

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

/** The range chosen first: a relative preset, or the absolute inputs. */
function resolveDomain() {
  if (state.mode === "absolute" && state.domainUntil > state.domainFrom) {
    return { from: state.domainFrom, until: state.domainUntil };
  }
  const until = nowNs();
  return { from: until - durationNs(state.range), until };
}

/** The bracket, clamped into the domain; falls back to the whole domain. */
function resolveWindow() {
  const domain = resolveDomain();
  if (!state.fromNs || !state.untilNs || state.untilNs <= state.fromNs) return domain;
  const span = Math.min(state.untilNs - state.fromNs, domain.until - domain.from);
  let from = state.fromNs;
  if (from < domain.from) from = domain.from;
  if (from + span > domain.until) from = domain.until - span;
  return { from, until: from + span };
}

/** RFC3339, the format the server's time parser accepts. */
function iso(ns) {
  return new Date(Number(ns) / 1e6).toISOString();
}

/** `2026-11-12T20:15:00` in the browser's zone, for <input type=datetime-local>. */
function toLocalInput(ns) {
  const date = new Date(Number(ns) / 1e6);
  const pad = (n) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

function fromLocalInput(value) {
  if (!value) return null;
  const ms = new Date(value).getTime();
  return Number.isFinite(ms) ? ms * 1e6 : null;
}

function setRelativeRange(preset) {
  state.mode = "relative";
  state.range = preset;
  state.domainFrom = 0;
  state.domainUntil = 0;
  state.fromNs = 0;
  state.untilNs = 0;
  state.baseFromNs = 0;
  state.baseUntilNs = 0;
  afterTimeChange();
}

function setAbsoluteDomain(from, until) {
  if (!(until > from)) {
    setBanner("`From` must be earlier than `To`.");
    return;
  }
  setBanner("");
  state.mode = "absolute";
  state.domainFrom = Math.round(from);
  state.domainUntil = Math.round(until);
  state.fromNs = state.domainFrom;
  state.untilNs = state.domainUntil;
  state.baseFromNs = 0;
  state.baseUntilNs = 0;
  afterTimeChange();
}

/** One entry point for "the chosen range changed": rebuild, persist, reload. */
function afterTimeChange() {
  lastDomain = resolveDomain();
  syncControls();
  writeHash();
  renderTimebar();
  load();
  loadHistogram();
}

function onWindowChange(from, until) {
  state.fromNs = from;
  state.untilNs = until;
  syncControls();
  writeHash();
  load();
}

function onBaselineChange(from, until) {
  state.baseFromNs = from;
  state.baseUntilNs = until;
  writeHash();
  load();
}

/** The window the baseline brush starts with, always inside the domain. */
function defaultBaseline() {
  const domain = resolveDomain();
  const win = resolveWindow();
  const span = win.until - win.from;
  const full = span >= (domain.until - domain.from) * 0.999;
  if (full) {
    // Nothing precedes a full-domain window: compare the two halves.
    const mid = Math.floor((domain.from + domain.until) / 2);
    return { from: domain.from, until: mid, split: mid };
  }
  let from = win.from - span;
  let until = win.from;
  if (from < domain.from) from = domain.from;
  if (until - from < span * 0.25) {
    from = domain.from;
    until = Math.min(domain.from + span, win.from);
  }
  return { from, until, split: null };
}

function resolveBaseline() {
  const domain = resolveDomain();
  if (state.baseFromNs && state.baseUntilNs > state.baseFromNs) {
    const span = Math.min(state.baseUntilNs - state.baseFromNs, domain.until - domain.from);
    let from = Math.max(domain.from, state.baseFromNs);
    if (from + span > domain.until) from = domain.until - span;
    return { from, until: Math.round(from + span) };
  }
  return defaultBaseline();
}

/**
 * Entering compare with a full-domain window has no baseline to show, so the
 * domain is split in half and the second half becomes the window.
 */
function prepareCompare() {
  const domain = resolveDomain();
  const win = resolveWindow();
  const full = win.until - win.from >= (domain.until - domain.from) * 0.999;
  if (!state.baseFromNs && full) {
    const split = defaultBaseline();
    if (split.split) {
      state.fromNs = split.split;
      state.untilNs = domain.until;
      state.baseFromNs = domain.from;
      state.baseUntilNs = split.split;
    }
  } else if (!state.baseFromNs) {
    const baseline = defaultBaseline();
    state.baseFromNs = baseline.from;
    state.baseUntilNs = baseline.until;
  }
}

// ---- routing -----------------------------------------------------------

function parseHash() {
  const raw = location.hash.replace(/^#\/?/, "");
  const [view, query] = raw.split("?");
  const params = new URLSearchParams(query ?? "");
  if (VIEWS.includes(view)) state.view = view;
  if (params.get("type")) state.type = params.get("type");

  const from = Number(params.get("from"));
  const until = Number(params.get("until"));
  if (Number.isFinite(from) && Number.isFinite(until) && until > from) {
    state.mode = "absolute";
    state.domainFrom = from;
    state.domainUntil = until;
  } else if (params.get("range")) {
    state.mode = "relative";
    state.range = params.get("range");
  }

  const wfrom = Number(params.get("wfrom"));
  const wuntil = Number(params.get("wuntil"));
  if (Number.isFinite(wfrom) && Number.isFinite(wuntil) && wuntil > wfrom) {
    state.fromNs = wfrom;
    state.untilNs = wuntil;
  } else {
    state.fromNs = 0;
    state.untilNs = 0;
  }

  const bfrom = Number(params.get("bwfrom"));
  const buntil = Number(params.get("bwuntil"));
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
  const domain = resolveDomain();
  if (state.mode === "absolute") {
    params.set("from", String(state.domainFrom));
    params.set("until", String(state.domainUntil));
  } else {
    params.set("range", state.range);
  }
  const win = resolveWindow();
  const whole =
    Math.abs(win.from - domain.from) < 1e6 && Math.abs(win.until - domain.until) < 1e6;
  if (!whole) {
    params.set("wfrom", String(win.from));
    params.set("wuntil", String(win.until));
  }
  if (state.view === "compare") {
    const baseline = resolveBaseline();
    params.set("bwfrom", String(baseline.from));
    params.set("bwuntil", String(baseline.until));
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
    from: iso(win.from),
    until: iso(win.until),
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
  if (state.view === "compare") prepareCompare();
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

// ---- time bar ----------------------------------------------------------

function renderTimebar() {
  brushes.main?.destroy();
  brushes.baseline?.destroy();
  brushes.baseline = null;
  el.brushes.innerHTML = "";

  const domain = resolveDomain();
  const win = resolveWindow();

  if (state.view === "compare") {
    const wrap = document.createElement("div");
    wrap.className = "brushes";
    el.brushes.appendChild(wrap);
    // One host per brush: a brush renders by replacing its host's innerHTML,
    // so two brushes in one host would clobber each other.
    const baselineHost = document.createElement("div");
    const mainHost = document.createElement("div");
    wrap.append(baselineHost, mainHost);

    const baseline = resolveBaseline();
    brushes.baseline = new TimeBrush(baselineHost, {
      label: "A · Baseline",
      onChange: onBaselineChange,
    });
    brushes.main = new TimeBrush(mainHost, {
      label: "B · Comparison",
      onChange: onWindowChange,
    });
    for (const brush of [brushes.baseline, brushes.main]) {
      brush.setDomain(domain.from, domain.until);
    }
    brushes.baseline.setSelection(baseline.from, baseline.until);
    brushes.main.setSelection(win.from, win.until);
    addFullButton(brushes.baseline);
    addFullButton(brushes.main);
  } else {
    brushes.main = new TimeBrush(el.brushes, {
      label: "Time range",
      onChange: onWindowChange,
    });
    brushes.main.setDomain(domain.from, domain.until);
    brushes.main.setSelection(win.from, win.until);
    addFullButton(brushes.main);
  }

  if (histData) feedBrushes();
}

/** Small "Full" action: put the bracket back to the whole domain. */
function addFullButton(brush) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "ghost brush-action";
  button.textContent = "Full";
  button.title = "Reset this window to the whole range";
  button.addEventListener("click", () => {
    const domain = resolveDomain();
    brush.onChange(domain.from, domain.until);
  });
  brush.headEl.appendChild(button);
}

function feedBrushes() {
  brushes.main?.setHistogram(histData);
  brushes.baseline?.setHistogram(histData);
}

/** Refresh the histogram over the domain; single-flighted. */
async function loadHistogram() {
  const token = ++histToken;
  const domain = resolveDomain();
  const selector = composeSelector(state.filters);
  const params = {
    profile_type: state.type,
    from: iso(domain.from),
    until: iso(domain.until),
    buckets: 120,
  };
  if (selector) params.selector = selector;
  try {
    const payload = await api.timeseries(params);
    if (token !== histToken) return;
    histData = payload;
    histEmpty = (payload.series ?? []).every((series) =>
      (series.points ?? []).every((point) => !point),
    );
    feedBrushes();
    updateHint();
  } catch {
    // The histogram is decoration on top of a working query path; a failure
    // here (token expiry, transient 5xx) must not blank the views.
  }
}

/** When the chosen range has no data, point at the range that does. */
function updateHint() {
  if (histEmpty && dataRange?.earliest && dataRange?.latest) {
    el.hint.textContent = `No samples here; data spans ${formatClock(
      dataRange.earliest,
    )} → ${formatClock(dataRange.latest)}`;
    el.hintUse.hidden = false;
  } else {
    el.hint.textContent = "";
    el.hintUse.hidden = true;
  }
}

async function loadStats() {
  try {
    const stats = await api.stats();
    const store = stats?.store ?? {};
    dataRange =
      store.earliest_ns && store.latest_ns
        ? { earliest: store.earliest_ns, latest: store.latest_ns }
        : null;
  } catch {
    dataRange = null;
  }
  updateHint();
}

// ---- label filters -----------------------------------------------------

async function loadValuesFor(label) {
  const others = composeSelector(
    state.filters.filter((filter) => !filter.raw && filter.label !== label),
  );
  const domain = resolveDomain();
  const key = `${state.tenant}|${label}|${others}|${domain.from}|${domain.until}`;
  if (valueCache.has(key)) return valueCache.get(key);

  const fetchValues = async (from, until) => {
    const params = { from: iso(from), until: iso(until) };
    if (others) params.selector = others;
    const result = await api.labelValues(label, params);
    return (result.values ?? []).map((row) => row.value);
  };

  const promise = (async () => {
    let values = [];
    try {
      values = await fetchValues(domain.from, domain.until);
    } catch {
      values = [];
    }
    if (values.length) return { values, note: "" };
    // An empty chosen range must not dead-end the picker: offer what exists
    // elsewhere, with a note saying so.
    try {
      const until = nowNs();
      const wide = await fetchValues(until - 7 * 86_400e9, until);
      if (wide.length) {
        return {
          values: wide,
          note: "No values in the selected range — showing the last 7 days",
        };
      }
    } catch {
      /* fall through with the empty list */
    }
    return { values, note: "No values found in the selected range" };
  })();

  valueCache.set(key, promise);
  return promise;
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

/** Mirror the state into the toolbar and the time picker. */
function syncControls() {
  el.range.value = state.mode === "absolute" ? "custom" : state.range;
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

  // Time picker: mode, context, absolute inputs.
  for (const button of el.rangeMode.querySelectorAll("button[data-mode]")) {
    button.classList.toggle("active", button.dataset.mode === state.mode);
  }
  el.absRange.hidden = state.mode !== "absolute";
  const domain = resolveDomain();
  el.absFrom.value = toLocalInput(domain.from);
  el.absUntil.value = toLocalInput(domain.until);
  if (state.mode === "absolute") {
    el.rangeContext.textContent = `absolute · ${domainSpanText(domain)}`;
  } else {
    const label = RANGES.find(([value]) => value === state.range)?.[1] ?? state.range;
    el.rangeContext.textContent = `${label.toLowerCase()} · ends now`;
  }

  if (el.filters.dataset.selector !== composeSelector(state.filters)) {
    filtersComponent.setFilters(state.filters);
  }
}

function domainSpanText(domain) {
  const span = domain.until - domain.from;
  const days = span / 86_400e9;
  if (days >= 1) return `${days.toFixed(days >= 10 ? 0 : 1)} days`;
  const hours = span / 3.6e12;
  if (hours >= 1) return `${hours.toFixed(hours >= 10 ? 0 : 1)} h`;
  return `${Math.round(span / 6e10)} min`;
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
      ? `Tenant "${state.tenant}" has no data in this range. Widen the range, or switch tenant.`
      : "Start an agent on a machine with eBPF enabled, or point an existing one here.",
    command,
  });
  component = null;
}

// ---- UI wiring ---------------------------------------------------------

function populateSelects() {
  el.range.innerHTML =
    RANGES.map(([value, label]) => `<option value="${value}">${label}</option>`).join("") +
    '<option value="custom">Custom (absolute range below)</option>';
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
    const result = await api.profileTypes({ from: iso(win.from), until: iso(win.until) });
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
  const picked = el.range.value;
  if (picked === "custom") {
    if (state.mode !== "absolute") {
      const win = resolveWindow();
      setAbsoluteDomain(win.from, win.until);
      return;
    }
  } else if (picked !== state.range || state.mode !== "relative") {
    setRelativeRange(picked);
    return;
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
  histData = null;
  dataRange = null;
  writeHash();
  loadStats();
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
  if (view === "compare") prepareCompare();
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
      // A relative domain follows the clock; the bracket moves with it so the
      // same sub-interval stays selected. An absolute domain is pinned.
      if (state.mode === "relative") {
        const domain = resolveDomain();
        const shift = lastDomain ? domain.until - lastDomain.until : 0;
        if (shift > 0) {
          if (state.fromNs) {
            state.fromNs += shift;
            state.untilNs += shift;
          }
          if (state.baseFromNs) {
            state.baseFromNs += shift;
            state.baseUntilNs += shift;
          }
        }
        lastDomain = domain;
      }
      renderTimebar();
      syncControls();
      load();
      loadHistogram();
    }, 30_000);
  }
}

function boot() {
  // `?tenant=acme` is the link form people paste around; the hash carries it
  // too, but the query string is what a bookmark or a chat message tends to
  // have. Read it before the hash so an explicit hash param still wins.
  const queryTenant = new URLSearchParams(location.search).get("tenant");
  if (queryTenant) {
    state.tenant = queryTenant;
    setTenant(queryTenant);
  }
  populateSelects();
  parseHash();
  lastDomain = resolveDomain();
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
    lastDomain = resolveDomain();
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

  el.rangeMode.addEventListener("click", (event) => {
    const button = event.target.closest("button[data-mode]");
    if (!button || button.dataset.mode === state.mode) return;
    if (button.dataset.mode === "relative") {
      setRelativeRange(state.range === "custom" ? "1h" : state.range);
    } else {
      // Switching to absolute keeps the range you are looking at, so the
      // picker opens with something meaningful in it.
      const win = resolveWindow();
      setAbsoluteDomain(win.from, win.until);
    }
  });
  el.absApply.addEventListener("click", () => {
    const from = fromLocalInput(el.absFrom.value);
    const until = fromLocalInput(el.absUntil.value);
    if (from === null || until === null) {
      setBanner("Pick both ends of the range.");
      return;
    }
    setAbsoluteDomain(from, until);
  });
  el.absNow.addEventListener("click", () => {
    const from = fromLocalInput(el.absFrom.value) ?? resolveDomain().from;
    setAbsoluteDomain(from, nowNs());
  });
  for (const input of [el.absFrom, el.absUntil]) {
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") el.absApply.click();
    });
  }
  el.hintUse.addEventListener("click", () => {
    if (!dataRange) return;
    setAbsoluteDomain(dataRange.earliest, dataRange.latest);
  });

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
    loadStats();
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
  setAbsoluteDomain,
  setRelativeRange,
};

boot();
