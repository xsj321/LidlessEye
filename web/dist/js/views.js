// The table-shaped views: "top functions", the overview and the series list.

import {
  escapeHtml,
  formatBytes,
  formatCount,
  formatLabels,
  formatPercent,
  formatRelative,
  formatTimestamp,
  formatValue,
} from "./format.js";

/** Sortable, filterable "top functions" table. */
export class TopTable {
  constructor(host) {
    this.host = host;
    this.items = [];
    this.unit = "nanoseconds";
    this.sortKey = "self_value";
    this.ascending = false;
    this.filter = "";
    this.total = 0;

    host.classList.add("panel");
    host.innerHTML = `
      <div class="fg-toolbar">
        <input type="search" placeholder="Filter functions…" aria-label="Filter functions" />
        <span class="muted" data-role="count"></span>
        <span class="spacer"></span>
        <span class="faint">Click a column header to sort.</span>
      </div>
      <div class="tbl-wrap"><table>
        <thead><tr>
          <th data-key="name">Function</th>
          <th data-key="self_value" class="num">Self</th>
          <th data-key="self_value" class="bar-cell"></th>
          <th data-key="total_value" class="num">Total</th>
          <th data-key="name" class="num">Total %</th>
        </tr></thead>
        <tbody></tbody>
      </table></div>
    `;
    this.tbody = host.querySelector("tbody");
    this.countEl = host.querySelector('[data-role="count"]');
    host.querySelectorAll("th").forEach((th) => {
      th.addEventListener("click", () => {
        const key = th.dataset.key;
        if (!key) return;
        if (this.sortKey === key) this.ascending = !this.ascending;
        else {
          this.sortKey = key;
          this.ascending = key === "name";
        }
        this.render();
      });
    });
    const search = host.querySelector('input[type="search"]');
    search.addEventListener("input", () => {
      this.filter = search.value.trim().toLowerCase();
      this.render();
    });
  }

  setData(payload) {
    this.items = payload.items ?? [];
    this.unit = payload.unit ?? "nanoseconds";
    this.total = payload.total_value ?? 0;
    this.render();
  }

  rows() {
    const filtered = this.filter
      ? this.items.filter(
          (row) =>
            row.name.toLowerCase().includes(this.filter) ||
            (row.module ?? "").toLowerCase().includes(this.filter) ||
            (row.file ?? "").toLowerCase().includes(this.filter),
        )
      : this.items.slice();
    const key = this.sortKey;
    filtered.sort((a, b) => {
      if (key === "name") {
        const cmp = a.name.localeCompare(b.name);
        return this.ascending ? cmp : -cmp;
      }
      const cmp = (a[key] ?? 0) - (b[key] ?? 0);
      return this.ascending ? cmp : -cmp;
    });
    return filtered;
  }

  render() {
    const rows = this.rows();
    const maxSelf = rows.reduce((acc, row) => Math.max(acc, row.self_value ?? 0), 0) || 1;
    const total = this.total || this.items.reduce((acc, row) => acc + (row.self_value ?? 0), 0) || 1;
    this.countEl.textContent = `${rows.length} of ${this.items.length} functions`;
    this.tbody.innerHTML = rows
      .map((row) => {
        const location = [row.module, row.file && row.line ? `${row.file}:${row.line}` : row.file]
          .filter(Boolean)
          .join(" · ");
        const width = Math.max(1, Math.round(((row.self_value ?? 0) / maxSelf) * 100));
        const share = (row.total_value ?? 0) / total;
        return `<tr>
          <td class="name" title="${escapeHtml(location)}">${escapeHtml(row.name)}${
            location ? `<span class="sub">${escapeHtml(location)}</span>` : ""
          }</td>
          <td class="num">${formatValue(row.self_value, this.unit)}</td>
          <td class="bar-cell"><div class="bar" style="width:${width}%"></div></td>
          <td class="num">${formatValue(row.total_value, this.unit)}</td>
          <td class="num muted">${formatPercent(share)}</td>
        </tr>`;
      })
      .join("");
  }
}

/** Overview: storage stats, available profile types and known series. */
/** `1 report ingested` / `2 reports ingested`, without an "s" on the first. */
function plural(count, one, many) {
  return `${formatCount(count)} ${count === 1 ? one : many}`;
}

export function renderOverview(host, { stats, types, series }) {
  const store = stats?.store ?? {};
  const cards = [
    ["Uptime", formatUptime(stats?.uptime_seconds ?? 0), `version ${stats?.version ?? "?"}`],
    ["Batches", formatCount(store.segment_count ?? 0), formatBytes(store.total_bytes ?? 0)],
    [
      "Series",
      formatCount(stats?.series ?? 0),
      plural(stats?.ingest_reports ?? 0, "report ingested", "reports ingested"),
    ],
    [
      "Samples ingested",
      formatCount(stats?.ingest_samples ?? 0),
      plural(stats?.queries ?? 0, "query served", "queries served"),
    ],
    [
      "Oldest data",
      store.earliest_ns ? formatRelative(store.earliest_ns) : "–",
      store.latest_ns ? `newest ${formatRelative(store.latest_ns)}` : "",
    ],
  ];

  const typeCards = (types ?? [])
    .map(
      (type) => `<div class="card">
        <div class="k">${escapeHtml(type.name)}${type.virtual ? " (virtual)" : ""}</div>
        <div class="v">${formatValue(type.total_value, type.unit)}</div>
        <div class="s">${type.segment_count} batches · ${escapeHtml(type.unit)}</div>
      </div>`,
    )
    .join("");

  const rows = (series ?? [])
    .map(
      (row) => `<tr>
        <td>${escapeHtml(row.service_name)}</td>
        <td class="mono">${escapeHtml(row.profile_type)}</td>
        <td class="mono muted">${escapeHtml(formatLabels(row.labels) || "–")}</td>
        <td class="num">${formatCount(row.total_value)}</td>
        <td class="num">${formatCount(row.segment_count)}</td>
        <td class="muted">${escapeHtml(formatTimestamp(row.start_ns))} → ${escapeHtml(formatTimestamp(row.end_ns))}</td>
      </tr>`,
    )
    .join("");

  host.innerHTML = `
    <div class="cards">
      ${cards
        .map(
          ([k, v, s]) => `<div class="card"><div class="k">${k}</div><div class="v">${v}</div><div class="s">${s}</div></div>`,
        )
        .join("")}
    </div>
    ${typeCards ? `<div class="cards">${typeCards}</div>` : ""}
    <div class="panel">
      <div class="panel-head"><h2>Series</h2><span class="spacer"></span>
        <span class="faint">${(series ?? []).length} combinations in the selected range</span></div>
      <div class="tbl-wrap"><table>
        <thead><tr><th>Service</th><th>Profile</th><th>Labels</th><th class="num">Value</th><th class="num">Batches</th><th>Range</th></tr></thead>
        <tbody>${rows || `<tr><td colspan="6" class="empty">No series yet.</td></tr>`}</tbody>
      </table></div>
    </div>
  `;
}

function formatUptime(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return "–";
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days) return `${days}d ${hours}h`;
  if (hours) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

/** Placeholder shown when a query returns nothing. */
export function emptyState(host, { title, hint, command }) {
  host.innerHTML = `
    <div class="empty">
      <h3>${escapeHtml(title)}</h3>
      <p>${escapeHtml(hint)}</p>
      ${command ? `<code>${escapeHtml(command)}</code>` : ""}
    </div>
  `;
}

/**
 * Tag explorer: the label names on the left, the values of the selected label
 * on the right. Clicking a value narrows the selector, which is how you go from
 * "something is slow" to "it is slow on this pod".
 */
export class LabelExplorer {
  constructor(host, { onFilter } = {}) {
    this.host = host;
    this.onFilter = onFilter ?? (() => {});
    this.names = [];
    this.active = null;
    this.values = [];
    this.search = "";

    host.classList.add("panel");
    host.innerHTML = `
      <div class="fg-toolbar">
        <input type="search" placeholder="Filter values…" aria-label="Filter label values" />
        <span class="muted" data-role="count"></span>
        <span class="spacer"></span>
        <span class="faint">Click a value to add it to the selector.</span>
      </div>
      <div class="labels-layout">
        <aside class="label-names" data-role="names"></aside>
        <div class="label-values" data-role="values"></div>
      </div>
    `;
    this.namesEl = host.querySelector('[data-role="names"]');
    this.valuesEl = host.querySelector('[data-role="values"]');
    this.countEl = host.querySelector('[data-role="count"]');
    const search = host.querySelector('input[type="search"]');
    search.addEventListener("input", () => {
      this.search = search.value.trim().toLowerCase();
      this.renderValues();
    });
    this.namesEl.addEventListener("click", (event) => {
      const button = event.target.closest("button[data-name]");
      if (!button) return;
      this.select(button.dataset.name);
    });
    this.valuesEl.addEventListener("click", (event) => {
      const button = event.target.closest("button[data-value]");
      if (!button) return;
      this.onFilter(this.active, button.dataset.value);
    });
  }

  setNames(names) {
    this.names = names ?? [];
    if (!this.active || !this.names.includes(this.active)) {
      this.active = this.names.find((name) => name !== "service_name") ?? this.names[0] ?? null;
    }
    this.renderNames();
  }

  select(name) {
    if (!name || name === this.active) return;
    this.active = name;
    this.values = [];
    this.renderNames();
    this.renderValues();
    this.onSelect?.(name);
  }

  setValues(label, values) {
    if (label !== this.active) return;
    this.values = values ?? [];
    this.renderValues();
  }

  renderNames() {
    this.namesEl.innerHTML = this.names
      .map(
        (name) => `<button data-name="${escapeHtml(name)}" class="${
          name === this.active ? "active" : ""
        }">${escapeHtml(name)}</button>`,
      )
      .join("");
  }

  renderValues() {
    if (!this.active) {
      this.valuesEl.innerHTML = `<div class="empty"><h3>No labels</h3><p>Ingest a profile first.</p></div>`;
      this.countEl.textContent = "";
      return;
    }
    const filtered = this.search
      ? this.values.filter((row) => row.value.toLowerCase().includes(this.search))
      : this.values;
    const max = filtered.reduce((acc, row) => Math.max(acc, row.total ?? 0), 0) || 1;
    this.countEl.textContent = `${filtered.length} value${filtered.length === 1 ? "" : "s"} for ${this.active}`;
    this.valuesEl.innerHTML = `
      <table class="label-table">
        <thead><tr><th>Value</th><th class="bar-cell"></th><th class="num">Total</th><th></th></tr></thead>
        <tbody>
          ${
            filtered
              .map(
                (row) => `<tr>
                  <td class="mono">${escapeHtml(row.value)}</td>
                  <td class="bar-cell"><div class="bar" style="width:${Math.max(
                    1,
                    Math.round((row.total / max) * 100),
                  )}%"></div></td>
                  <td class="num">${formatCount(row.total)}</td>
                  <td class="num"><button class="ghost" data-value="${escapeHtml(
                    row.value,
                  )}">Filter</button></td>
                </tr>`,
              )
              .join("") || `<tr><td colspan="4" class="empty">No values.</td></tr>`
          }
        </tbody>
      </table>
    `;
  }
}

