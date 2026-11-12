// Structured label filters: one row per matcher, composed into the selector
// string the API already understands.
//
// Each row is [label name] [operator] [value], all dropdowns: names come from
// /api/v1/label-names, values from /api/v1/label/<name>/values (scoped by the
// other rows — app.js decides how). Rows can be added and removed freely; the
// composed selector is mirrored into a read-only preview so a deep link and
// what the server sees stay visible.

const OPS = [
  ["=", "is"],
  ["!=", "is not"],
  ["=~", "matches"],
  ["!~", "does not match"],
];

export class LabelFilters {
  /**
   * @param {HTMLElement} host
   * @param {{ loadValues: (label: string) => Promise<string[]>, onChange: (filters: Filter[]) => void }} options
   */
  constructor(host, { loadValues, onChange } = {}) {
    this.host = host;
    this.loadValues = loadValues ?? (async () => []);
    this.onChange = onChange ?? (() => {});
    this.names = [];
    this.filters = [];

    host.classList.add("filters");
    host.innerHTML = `
      <button class="ghost filter-add" data-role="add">+ Filter</button>
      <span class="filters-preview mono" data-role="preview" hidden></span>
    `;
    this.addEl = host.querySelector('[data-role="add"]');
    this.previewEl = host.querySelector('[data-role="preview"]');

    this.addEl.addEventListener("click", () => {
      const used = new Set(this.filters.map((f) => f.label));
      const name = this.names.find((n) => !used.has(n)) ?? this.names[0] ?? "service_name";
      this.filters.push({ label: name, op: "=", value: "" });
      this.render();
      this.emit();
    });
  }

  destroy() {
    this.host.innerHTML = "";
  }

  setNames(names) {
    this.names = names ?? [];
    for (const select of this.host.querySelectorAll('[data-role="label"]')) {
      refreshSelect(select, this.names, select.value);
    }
  }

  setFilters(filters) {
    this.filters = (filters ?? []).map((f) => ({ ...f }));
    this.render();
  }

  emit() {
    const text = composeSelector(this.filters);
    this.host.dataset.selector = text;
    if (text) {
      this.previewEl.hidden = false;
      this.previewEl.textContent = text;
    } else {
      this.previewEl.hidden = true;
    }
    this.onChange(this.filters.map((f) => ({ ...f })));
  }

  async loadRowValues(index) {
    const row = this.host.querySelector(`.filter-row[data-index="${index}"]`);
    const filter = this.filters[index];
    if (!row || !filter?.label) return;
    const valueSelect = row.querySelector('[data-role="value"]');
    valueSelect.disabled = true;
    let result;
    try {
      result = await this.loadValues(filter.label);
    } catch {
      result = { values: [] }; // a failed lookup must not wedge the row
    }
    const values = Array.isArray(result) ? result : (result?.values ?? []);
    const note = Array.isArray(result) ? "" : (result?.note ?? "");
    // The row may have been re-rendered while the request was in flight.
    const still = this.host.querySelector(`.filter-row[data-index="${index}"]`);
    if (still !== row) return;
    refreshSelect(valueSelect, values, filter.value);
    valueSelect.disabled = false;
    valueSelect.title = note || "Label value";
  }

  render() {
    for (const row of [...this.host.querySelectorAll(".filter-row")]) row.remove();
    this.filters.forEach((filter, index) => {
      const row = document.createElement("span");
      row.className = "filter-row";
      row.dataset.index = String(index);
      if (filter.raw !== undefined) {
        // Pasted matcher: show it, let it apply, but do not pretend it is
        // editable with dropdowns it cannot round-trip through.
        row.innerHTML = `
          <span class="filter-raw mono" title="Selector expression kept as written"></span>
          <button class="filter-remove" data-role="remove" title="Remove this filter">×</button>
        `;
        row.querySelector(".filter-raw").textContent = filter.raw;
        row.querySelector('[data-role="remove"]').addEventListener("click", () => {
          this.filters.splice(index, 1);
          this.render();
          this.filters.forEach((_, i) => this.loadRowValues(i));
          this.emit();
        });
        this.host.insertBefore(row, this.addEl);
        return;
      }
      row.innerHTML = `
        <select data-role="label" title="Label name"></select>
        <select data-role="op" title="Operator">
          ${OPS.map(([value, label]) => `<option value="${value}">${label}</option>`).join("")}
        </select>
        <select data-role="value" title="Label value"></select>
        <button class="filter-remove" data-role="remove" title="Remove this filter">×</button>
      `;
      refreshSelect(row.querySelector('[data-role="label"]'), this.names, filter.label);
      refreshSelect(row.querySelector('[data-role="value"]'), [], filter.value);
      row.querySelector('[data-role="op"]').value = filter.op || "=";
      this.host.insertBefore(row, this.addEl);

      row.querySelector('[data-role="label"]').addEventListener("change", (event) => {
        filter.label = event.target.value;
        filter.value = "";
        this.render();
        this.emit();
      });
      row.querySelector('[data-role="op"]').addEventListener("change", (event) => {
        filter.op = event.target.value;
        this.emit();
      });
      row.querySelector('[data-role="value"]').addEventListener("change", (event) => {
        filter.value = event.target.value;
        this.emit();
      });
      row.querySelector('[data-role="remove"]').addEventListener("click", () => {
        this.filters.splice(index, 1);
        this.render();
        this.emit();
      });
    });
    const text = composeSelector(this.filters);
    this.host.dataset.selector = text;
    if (text) {
      this.previewEl.hidden = false;
      this.previewEl.textContent = text;
    } else {
      this.previewEl.hidden = true;
    }
    // Every row reloads its list: a re-render replaces all the selects, and a
    // row left with only its current value looks like "no values found".
    this.filters.forEach((_, index) => this.loadRowValues(index));
  }
}

/** `{pod="x", service_name=~"api-*"}` — braces optional on the server. */
export function composeSelector(filters) {
  const parts = [];
  for (const filter of filters ?? []) {
    if (filter.raw) {
      // A matcher the UI could not break into row controls (pasted from a
      // deep link); it is kept verbatim so the filter still applies.
      parts.push(filter.raw);
      continue;
    }
    if (filter.label && filter.value && filter.op) {
      parts.push(`${filter.label}${filter.op}"${escapeValue(filter.value)}"`);
    }
  }
  return parts.join(",");
}

/** Parse a selector back into rows; whatever does not fit becomes a raw row. */
export function parseSelector(text) {
  const input = String(text ?? "").trim();
  if (!input) return [];
  const filters = [];
  let rest = input;
  if (rest.startsWith("{") && rest.endsWith("}")) rest = rest.slice(1, -1);
  for (const part of splitTopLevel(rest)) {
    const matcher = /^([^\s=!~]+)\s*(=|!=|=~|!~)\s*"((?:[^"\\]|\\.)*)"$/.exec(part.trim());
    if (matcher) {
      filters.push({
        label: matcher[1],
        op: matcher[2],
        value: unescapeValue(matcher[3]),
      });
    } else {
      // Not a matcher we can edit as a row: keep it verbatim so a deep link
      // someone pasted still filters the same way.
      filters.push({ raw: part.trim() });
    }
  }
  return filters;
}

/** Split on commas that sit outside double quotes (the server's rule). */
function splitTopLevel(text) {
  const parts = [];
  let current = "";
  let quoted = false;
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i];
    if (quoted) {
      current += c;
      if (c === "\\") {
        i += 1;
        current += text[i] ?? "";
      } else if (c === '"') {
        quoted = false;
      }
    } else if (c === '"') {
      quoted = true;
      current += c;
    } else if (c === ",") {
      parts.push(current);
      current = "";
    } else {
      current += c;
    }
  }
  if (current.trim()) parts.push(current);
  return parts.filter((part) => part.trim().length > 0);
}

function escapeValue(value) {
  return String(value).replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

function unescapeValue(value) {
  return String(value).replace(/\\(.)/g, "$1");
}

function refreshSelect(select, values, current) {
  select.innerHTML = values
    .map((value) => `<option value="${escapeHtml(String(value))}">${escapeHtml(value)}</option>`)
    .join("");
  if (current && !values.includes(current)) {
    const option = document.createElement("option");
    option.value = current;
    option.textContent = current;
    select.appendChild(option);
  }
  select.value = current ?? "";
}

function escapeHtml(text) {
  return String(text).replace(/[&<>"']/g, (c) =>
    c === "&" ? "&amp;" : c === "<" ? "&lt;" : c === ">" ? "&gt;" : c === '"' ? "&quot;" : "&#39;",
  );
}
