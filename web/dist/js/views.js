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
export function renderOverview(host, { stats, types, series }) {
  const store = stats?.store ?? {};
  const cards = [
    ["Uptime", formatUptime(stats?.uptime_seconds ?? 0), `version ${stats?.version ?? "?"}`],
    ["Segments", formatCount(store.segment_count ?? 0), formatBytes(store.total_bytes ?? 0)],
    ["Series", formatCount(stats?.series ?? 0), `${formatCount(stats?.ingest_reports ?? 0)} reports ingested`],
    ["Samples ingested", formatCount(stats?.ingest_samples ?? 0), `${formatCount(stats?.queries ?? 0)} queries served`],
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
        <div class="s">${type.segment_count} segments · ${escapeHtml(type.unit)}</div>
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
        <thead><tr><th>Service</th><th>Profile</th><th>Labels</th><th class="num">Value</th><th class="num">Segments</th><th>Range</th></tr></thead>
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
