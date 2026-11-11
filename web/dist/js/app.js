// Application shell: filter bar, hash routing, data loading.

import { ApiError, api } from "./api.js";
import { TimeSeriesChart } from "./chart.js";
import { Flamegraph } from "./flamegraph.js";
import { TopTable, emptyState, renderOverview } from "./views.js";

const RANGES = [
  ["5m", "Last 5 minutes"],
  ["15m", "Last 15 minutes"],
  ["1h", "Last hour"],
  ["6h", "Last 6 hours"],
  ["24h", "Last 24 hours"],
  ["7d", "Last 7 days"],
];

const state = {
  view: "flamegraph",
  type: "cpu",
  range: "1h",
  selector: "",
  auto: false,
  unit: "nanoseconds",
};

const el = {
  main: document.getElementById("main"),
  tabs: document.getElementById("tabs"),
  profileType: document.getElementById("profile-type"),
  range: document.getElementById("range"),
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
  if (["flamegraph", "top", "timeseries", "overview"].includes(view)) state.view = view;
  if (params.get("type")) state.type = params.get("type");
  if (params.get("range")) state.range = params.get("range");
  if (params.has("selector")) state.selector = params.get("selector");
}

function writeHash() {
  const params = new URLSearchParams();
  params.set("type", state.type);
  params.set("range", state.range);
  if (state.selector) params.set("selector", state.selector);
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
  } catch (err) {
    if (id !== requestId) return;
    handleError(err);
  } finally {
    if (id === requestId) delete el.main.dataset.loading;
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
  el.selector.value = state.selector;
  if (![...el.profileType.options].some((option) => option.value === state.type)) {
    const option = document.createElement("option");
    option.value = state.type;
    option.textContent = state.type;
    el.profileType.appendChild(option);
  }
  el.profileType.value = state.type;
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
  const command = `lidlesseye collect --server http://${location.host} --frequency 99`;
  emptyState(el.main, {
    title: "No samples yet",
    hint: "Start an agent on a machine with eBPF enabled, or point an existing one here.",
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
  el.profileType.innerHTML = '<option value="cpu">cpu</option>';
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

function applyFromControls() {
  state.type = el.profileType.value || "cpu";
  state.range = el.range.value;
  state.selector = el.selector.value;
  writeHash();
  load();
}

function setView(view) {
  state.view = view;
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
  el.profileType.addEventListener("change", applyFromControls);
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

  loadProfileTypes().then(() => {
    el.profileType.value = state.type;
    load();
  });
}

// Exposed for debugging from the browser console and for the UI smoke tests.
window.lidlesseye = {
  state,
  view: () => component,
  reload: load,
};

boot();
