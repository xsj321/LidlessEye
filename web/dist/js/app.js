// Application shell: filter bar, hash routing, data loading.

import { ApiError, api, getTenant, setTenant } from "./api.js";
import { TimeSeriesChart } from "./chart.js";
import { Flamegraph } from "./flamegraph.js";
import { LabelExplorer, TopTable, emptyState, renderOverview } from "./views.js";

const RANGES = [
  ["5m", "Last 5 minutes"],
  ["15m", "Last 15 minutes"],
  ["1h", "Last hour"],
  ["6h", "Last 6 hours"],
  ["24h", "Last 24 hours"],
  ["7d", "Last 7 days"],
];

const BASELINES = [
  ["same", "Preceding window"],
  ["15m", "15 minutes"],
  ["1h", "1 hour"],
  ["6h", "6 hours"],
  ["24h", "24 hours"],
];

const VIEWS = ["flamegraph", "compare", "top", "timeseries", "labels", "overview"];

const state = {
  view: "flamegraph",
  type: "cpu",
  range: "1h",
  baseline: "same",
  selector: "",
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
  baseline: document.getElementById("baseline"),
  baselineField: document.getElementById("baseline-field"),
  selector: document.getElementById("selector"),
  auto: document.getElementById("auto-refresh"),
  apply: document.getElementById("apply"),
  refresh: document.getElementById("refresh"),
  banner: document.getElementById("banner"),
  build: document.getElementById("build-info"),
};

let component = null;
let autoTimer = null;
let requestId = 0;

// ---- routing -----------------------------------------------------------

function parseHash() {
  const raw = location.hash.replace(/^#\/?/, "");
  const [view, query] = raw.split("?");
  const params = new URLSearchParams(query ?? "");
  if (VIEWS.includes(view)) state.view = view;
  if (params.get("type")) state.type = params.get("type");
  if (params.get("range")) state.range = params.get("range");
  if (params.get("baseline")) state.baseline = params.get("baseline");
  if (params.has("selector")) state.selector = params.get("selector");
  if (params.has("group_by")) state.groupBy = params.get("group_by");
  if (params.has("tenant")) {
    state.tenant = params.get("tenant");
    setTenant(state.tenant);
  }
}

function writeHash() {
  const params = new URLSearchParams();
  params.set("type", state.type);
  params.set("range", state.range);
  if (state.view === "compare") params.set("baseline", state.baseline);
  if (state.selector) params.set("selector", state.selector);
  if (state.groupBy) params.set("group_by", state.groupBy);
  if (state.tenant) params.set("tenant", state.tenant);
  const next = `#/${state.view}?${params.toString()}`;
  if (location.hash !== next) history.replaceState(null, "", next);
}

// ---- data --------------------------------------------------------------

function queryParams() {
  const params = {
    profile_type: state.type,
    from: `now-${state.range}`,
    until: "now",
  };
  if (state.selector.trim()) params.selector = state.selector.trim();
  if (state.groupBy && state.view !== "compare") params.group_by = state.groupBy;
  return params;
}

/** `1h` / `30m` / `7d` in nanoseconds. */
function durationNs(text) {
  const match = /^(\d+)([smhd])$/.exec(String(text).trim());
  if (!match) return 3_600_000_000_000;
  const value = Number(match[1]);
  const scale = { s: 1e9, m: 6e10, h: 3.6e12, d: 8.64e13 }[match[2]];
  return value * scale;
}

function diffParams() {
  const params = queryParams();
  const rangeNs = durationNs(state.range);
  const baselineNs = state.baseline === "same" ? rangeNs : durationNs(state.baseline);
  params.baseline_until = `now-${state.range}`;
  params.baseline_from = `now-${nsToText(rangeNs + baselineNs)}`;
  return params;
}

function nsToText(ns) {
  if (ns % 8.64e13 === 0) return `${ns / 8.64e13}d`;
  if (ns % 3.6e12 === 0) return `${ns / 3.6e12}h`;
  if (ns % 6e10 === 0) return `${ns / 6e10}m`;
  return `${Math.round(ns / 1e9)}s`;
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

async function loadLabels(id, params) {
  const names = await api.labelNames(params);
  if (id !== requestId) return;
  if (!(component instanceof LabelExplorer)) {
    mount(
      (box) =>
        new LabelExplorer(box, {
          onFilter: (label, value) => {
            state.selector = `${label}="${value}"`;
            syncControls();
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
  el.baseline.value = state.baseline;
  el.baselineField.hidden = state.view !== "compare";
  el.selector.value = state.selector;
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
  el.range.innerHTML = RANGES.map(
    ([value, label]) => `<option value="${value}">${label}</option>`,
  ).join("");
  el.range.value = state.range;
  el.baseline.innerHTML = BASELINES.map(
    ([value, label]) => `<option value="${value}">${label}</option>`,
  ).join("");
  el.baseline.value = state.baseline;
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
    const result = await api.profileTypes({ from: `now-${state.range}`, until: "now" });
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
    const result = await api.labelNames({ from: `now-${state.range}`, until: "now" });
    const names = (result.label_names ?? []).filter((name) => name !== "service_name");
    el.groupBy.innerHTML =
      '<option value="">none</option>' +
      ["service_name", ...names]
        .map((name) => `<option value="${name}">${name}</option>`)
        .join("");
    el.groupBy.value = state.groupBy;
  } catch {
    /* grouping is optional */
  }
}

function applyFromControls() {
  state.type = el.profileType.value || "cpu";
  state.range = el.range.value;
  state.baseline = el.baseline.value;
  state.selector = el.selector.value;
  state.groupBy = el.groupBy.value;
  writeHash();
  load();
}

function switchTenant(tenant) {
  state.tenant = tenant;
  setTenant(tenant);
  state.label = null;
  writeHash();
  // Profile types and label names are per tenant, so reload them too.
  loadProfileTypes().then(() => {
    loadLabelNames();
    load();
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
  syncControls();
  writeHash();
  load();
}

function setupAutoRefresh() {
  state.auto = el.auto.checked;
  clearInterval(autoTimer);
  if (state.auto) autoTimer = setInterval(load, 30_000);
}

function boot() {
  populateSelects();
  parseHash();
  syncControls();
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
  el.refresh.addEventListener("click", load);
  el.auto.addEventListener("change", setupAutoRefresh);
  el.range.addEventListener("change", applyFromControls);
  el.baseline.addEventListener("change", applyFromControls);
  el.profileType.addEventListener("change", applyFromControls);
  el.groupBy.addEventListener("change", applyFromControls);
  el.tenant.addEventListener("change", () => switchTenant(el.tenant.value));
  el.selector.addEventListener("keydown", (event) => {
    if (event.key === "Enter") applyFromControls();
  });
  window.addEventListener("keydown", (event) => {
    if (event.target.matches("input, select, textarea")) return;
    if (event.key === "r" || event.key === "R") load();
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
    loadProfileTypes().then(() => {
      el.profileType.value = state.type;
      loadLabelNames();
      load();
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
