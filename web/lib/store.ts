// Dashboard state, data loading and URL synchronisation.
//
// This is the piece `app.js` used to be. It stays a plain class subscribed
// through `useSyncExternalStore` instead of becoming a tree of `useState`
// calls, because the interesting part is not "what changed" but the derivation
// chain: a chosen domain plus a bracket produce a query window, and that window
// produces both the request and the URL. Keeping it in one object is what makes
// a URL reproduce a view exactly.
//
// Every mutation goes through `commit()`, which shallow-copies the state (so
// `useSyncExternalStore` sees a new reference), rewrites the hash and notifies.

import { ApiError, api, getTenant, setTenant } from "./api";
import { buildHash, parseHash } from "./hash";
import type { HashPatch } from "./hash";
import { composeSelector, parseSelector } from "./selector";
import {
  defaultBaseline,
  domainSpanText,
  iso,
  nowNs,
  prepareCompare,
  rangeLabel,
  resolveBaseline,
  resolveDomain,
  resolveWindow,
  durationNs,
} from "./time";
import type { RangeMode, Window } from "./time";
import type {
  DiffPayload,
  FlamegraphPayload,
  LabelFilter,
  LabelValueRow,
  ProfileType,
  SeriesRow,
  StatsPayload,
  TimeseriesPayload,
  TopPayload,
  View,
} from "./types";

export interface Banner {
  tone: "error" | "warning";
  title: string;
  detail?: string;
}

export interface EmptyStateInfo {
  title: string;
  hint: string;
  command?: string;
}

export interface OverviewData {
  stats: StatsPayload;
  types: ProfileType[];
  series: SeriesRow[];
}

export interface DashboardState {
  view: View;
  type: string;
  mode: RangeMode;
  range: string;
  domainFrom: number;
  domainUntil: number;
  fromNs: number;
  untilNs: number;
  baseFromNs: number;
  baseUntilNs: number;
  filters: LabelFilter[];
  groupBy: string;
  tenant: string;
  auto: boolean;
  unit: string;

  ready: boolean;
  loading: boolean;
  banner: Banner | null;
  build: string;

  tenants: string[];
  profileTypes: string[];
  /** Label names over the last 7 days: what the toolbar and filters offer. */
  labelNames: string[];
  histData: TimeseriesPayload | null;
  histEmpty: boolean;
  dataRange: Window | null;

  flamegraph: FlamegraphPayload | DiffPayload | null;
  top: TopPayload | null;
  series: TimeseriesPayload | null;
  overview: OverviewData | null;

  /** Label explorer (own window-scoped name list, independent of the toolbar). */
  explorerNames: string[];
  activeLabel: string | null;
  explorerValues: LabelValueRow[];

  empty: EmptyStateInfo | null;
}

export interface FilterValues {
  values: string[];
  note: string;
}

function initialState(): DashboardState {
  return {
    view: "flamegraph",
    type: "cpu",
    mode: "relative",
    range: "1h",
    domainFrom: 0,
    domainUntil: 0,
    fromNs: 0,
    untilNs: 0,
    baseFromNs: 0,
    baseUntilNs: 0,
    filters: [],
    groupBy: "",
    tenant: "",
    auto: false,
    unit: "nanoseconds",

    ready: false,
    loading: false,
    banner: null,
    build: "",

    tenants: [],
    profileTypes: [],
    labelNames: [],
    histData: null,
    histEmpty: false,
    dataRange: null,

    flamegraph: null,
    top: null,
    series: null,
    overview: null,

    explorerNames: [],
    activeLabel: null,
    explorerValues: [],

    empty: null,
  };
}

const AUTO_REFRESH_MS = 30_000;

export class DashboardStore {
  private state: DashboardState = initialState();
  private readonly listeners = new Set<() => void>();

  private requestId = 0;
  private histToken = 0;
  private autoTimer: ReturnType<typeof setInterval> | null = null;
  private lastDomain: Window | null = null;
  private readonly valueCache = new Map<string, Promise<FilterValues>>();
  private booted = false;

  // ---- subscription -----------------------------------------------------

  getSnapshot = (): DashboardState => this.state;

  getServerSnapshot = (): DashboardState => this.state;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  private commit(patch: Partial<DashboardState>) {
    this.state = { ...this.state, ...patch };
    this.persistHash();
    for (const listener of this.listeners) listener();
  }

  // ---- time helpers -----------------------------------------------------

  /** Everything the time module needs, read from the current state. */
  private timeState() {
    const s = this.state;
    return {
      mode: s.mode,
      range: s.range,
      domainFrom: s.domainFrom,
      domainUntil: s.domainUntil,
      fromNs: s.fromNs,
      untilNs: s.untilNs,
      baseFromNs: s.baseFromNs,
      baseUntilNs: s.baseUntilNs,
    };
  }

  resolveDomain(): Window {
    return resolveDomain(this.timeState());
  }

  resolveWindow(): Window {
    return resolveWindow(this.timeState());
  }

  resolveBaseline(): Window {
    return resolveBaseline(this.timeState());
  }

  defaultBaseline() {
    return defaultBaseline(this.timeState());
  }

  domainSpanText(): string {
    return domainSpanText(this.resolveDomain());
  }

  rangeLabel(): string {
    return rangeLabel(this.state.range);
  }

  // ---- boot -------------------------------------------------------------

  /** Idempotent: React strict mode mounts effects twice in development. */
  initialize() {
    if (this.booted) return;
    this.booted = true;

    // `?tenant=acme` is the link form people paste around; the hash carries it
    // too, but the query string is what a bookmark or a chat message tends to
    // have. Read it before the hash so an explicit hash param still wins.
    const queryTenant = new URLSearchParams(window.location.search).get("tenant");
    let tenant = getTenant();
    if (queryTenant) {
      tenant = queryTenant;
      setTenant(queryTenant);
    }

    this.state = { ...this.state, tenant };

    const patch = parseHash(window.location.hash);
    this.applyPatch(patch);
    this.lastDomain = this.resolveDomain();

    window.addEventListener("hashchange", this.onHashChange);
    window.addEventListener("keydown", this.onGlobalKeyDown);

    void this.bootstrap();
  }

  destroy() {
    window.removeEventListener("hashchange", this.onHashChange);
    window.removeEventListener("keydown", this.onGlobalKeyDown);
    if (this.autoTimer) clearInterval(this.autoTimer);
    this.autoTimer = null;
    this.booted = false;
  }

  private onHashChange = () => {
    const patch = parseHash(window.location.hash);
    this.applyPatch(patch);
    this.lastDomain = this.resolveDomain();
    this.setView(this.state.view);
  };

  /** `R` reloads, like the button. Ignored while typing in a control. */
  private onGlobalKeyDown = (event: KeyboardEvent) => {
    if ((event.target as HTMLElement | null)?.matches?.("input, select, textarea")) return;
    if (event.key === "r" || event.key === "R") {
      this.reload();
      void this.loadHistogram();
    }
  };

  private async bootstrap() {
    await this.loadTenants();
    void this.loadStats();
    await this.loadProfileTypes();
    await this.loadLabelNames();
    this.commit({ ready: true });
    void this.reload();
    void this.loadHistogram();

    try {
      const info = await api.version();
      this.commit({ build: `v${info.version}` });
    } catch {
      this.commit({ build: "offline" });
    }
  }

  private applyPatch(patch: HashPatch) {
    if (patch.view) this.state = { ...this.state, view: patch.view };
    if (patch.type) this.state = { ...this.state, type: patch.type };
    if (patch.mode) this.state = { ...this.state, mode: patch.mode };
    if (patch.range) this.state = { ...this.state, range: patch.range };
    if (patch.domainFrom !== undefined) this.state = { ...this.state, domainFrom: patch.domainFrom };
    if (patch.domainUntil !== undefined) this.state = { ...this.state, domainUntil: patch.domainUntil };
    if (patch.fromNs !== undefined) this.state = { ...this.state, fromNs: patch.fromNs };
    if (patch.untilNs !== undefined) this.state = { ...this.state, untilNs: patch.untilNs };
    if (patch.baseFromNs !== undefined) this.state = { ...this.state, baseFromNs: patch.baseFromNs };
    if (patch.baseUntilNs !== undefined) this.state = { ...this.state, baseUntilNs: patch.baseUntilNs };
    if (patch.filters) this.state = { ...this.state, filters: patch.filters };
    if (patch.groupBy !== undefined) this.state = { ...this.state, groupBy: patch.groupBy };
    if (patch.tenant) {
      this.state = { ...this.state, tenant: patch.tenant };
      setTenant(patch.tenant);
    }
  }

  private persistHash() {
    const next = buildHash({
      view: this.state.view,
      type: this.state.type,
      filters: this.state.filters,
      groupBy: this.state.groupBy,
      tenant: this.state.tenant,
      ...this.timeState(),
    });
    if (window.location.hash !== next) history.replaceState(null, "", next);
  }

  // ---- view -------------------------------------------------------------

  setView(view: View) {
    const wasCompare = this.state.view === "compare";
    this.state = {
      ...this.state,
      view,
      activeLabel: null,
      empty: null,
      // Drop the previous view's payload. Keeping it would flash a table of
      // the old window under the new tab until the request lands — and a
      // `tbody tr` that exists is indistinguishable from freshly loaded data.
      flamegraph: null,
      top: null,
      series: null,
      overview: null,
      explorerNames: [],
      explorerValues: [],
    };

    if (view === "compare" && !wasCompare) {
      const patched = prepareCompare(this.timeState());
      if (patched) {
        this.state = { ...this.state, ...patched };
      }
    }
    this.commit({});
    void this.reload();
    void this.loadHistogram();
  }

  // ---- toolbar actions --------------------------------------------------

  setTenantId(tenant: string) {
    if (tenant === this.state.tenant) return;
    setTenant(tenant);
    this.valueCache.clear();
    this.commit({
      tenant,
      activeLabel: null,
      histData: null,
      dataRange: null,
      flamegraph: null,
      top: null,
      series: null,
      overview: null,
      empty: null,
    });
    void this.loadStats();
    void this.loadProfileTypes().then(() => {
      void this.loadLabelNames();
      void this.reload();
      void this.loadHistogram();
    });
  }

  setType(type: string) {
    this.commit({ type });
    void this.reload();
    void this.loadHistogram();
  }

  setGroupBy(groupBy: string) {
    this.commit({ groupBy });
    void this.reload();
    void this.loadHistogram();
  }

  setAuto(auto: boolean) {
    this.commit({ auto });
    if (this.autoTimer) clearInterval(this.autoTimer);
    this.autoTimer = null;
    if (!auto) return;
    this.autoTimer = setInterval(() => {
      // A relative domain follows the clock; the bracket moves with it so the
      // same sub-interval stays selected. An absolute domain is pinned.
      if (this.state.mode === "relative") {
        const domain = this.resolveDomain();
        const shift = this.lastDomain ? domain.until - this.lastDomain.until : 0;
        if (shift > 0) {
          const next: Partial<DashboardState> = {};
          if (this.state.fromNs) {
            next.fromNs = this.state.fromNs + shift;
            next.untilNs = this.state.untilNs + shift;
          }
          if (this.state.baseFromNs) {
            next.baseFromNs = this.state.baseFromNs + shift;
            next.baseUntilNs = this.state.baseUntilNs + shift;
          }
          this.state = { ...this.state, ...next };
        }
        this.lastDomain = domain;
      }
      this.commit({});
      void this.reload();
      void this.loadHistogram();
    }, AUTO_REFRESH_MS);
  }

  setRelativeRange(preset: string) {
    this.state = {
      ...this.state,
      mode: "relative",
      range: preset,
      domainFrom: 0,
      domainUntil: 0,
      fromNs: 0,
      untilNs: 0,
      baseFromNs: 0,
      baseUntilNs: 0,
    };
    this.afterTimeChange();
  }

  setAbsoluteDomain(from: number, until: number) {
    if (!(until > from)) {
      this.setBanner({ tone: "warning", title: "`From` must be earlier than `To`." });
      return;
    }
    this.setBanner(null);
    this.state = {
      ...this.state,
      mode: "absolute",
      domainFrom: Math.round(from),
      domainUntil: Math.round(until),
      fromNs: Math.round(from),
      untilNs: Math.round(until),
      baseFromNs: 0,
      baseUntilNs: 0,
    };
    this.afterTimeChange();
  }

  /** Switched with the Relative/Absolute control. */
  setMode(mode: RangeMode) {
    if (mode === this.state.mode) return;
    if (mode === "relative") {
      this.setRelativeRange(this.state.range === "custom" ? "1h" : this.state.range);
      return;
    }
    // Switching to absolute keeps the range you are looking at, so the picker
    // opens with something meaningful in it.
    const win = this.resolveWindow();
    this.setAbsoluteDomain(win.from, win.until);
  }

  private afterTimeChange() {
    this.lastDomain = this.resolveDomain();
    this.commit({});
    void this.reload();
    void this.loadHistogram();
  }

  /** A brush gesture finished: this is the window queries use. */
  setWindow(from: number, until: number) {
    this.commit({ fromNs: from, untilNs: until });
    void this.reload();
  }

  setBaseline(from: number, until: number) {
    this.commit({ baseFromNs: from, baseUntilNs: until });
    void this.reload();
  }

  resetWindowToDomain() {
    const domain = this.resolveDomain();
    this.setWindow(domain.from, domain.until);
  }

  // ---- filters ----------------------------------------------------------

  setFilters(filters: LabelFilter[]) {
    this.valueCache.clear();
    this.commit({ filters });
    void this.reload();
    void this.loadHistogram();
  }

  /** Turn one matcher into the whole selector, then jump to the flamegraph. */
  filterByLabel(label: string, value: string) {
    this.valueCache.clear();
    this.commit({ filters: [{ label, op: "=", value }], activeLabel: label });
    this.setView("flamegraph");
  }

  async filterValues(label: string): Promise<FilterValues> {
    const others = composeSelector(
      this.state.filters.filter((filter) => !filter.raw && filter.label !== label),
    );
    const domain = this.resolveDomain();
    const key = `${this.state.tenant}|${label}|${others}|${domain.from}|${domain.until}`;
    const cached = this.valueCache.get(key);
    if (cached) return cached;

    const fetchValues = async (from: number, until: number) => {
      const params: Record<string, string> = { from: iso(from), until: iso(until) };
      if (others) params.selector = others;
      const result = await api.labelValues(label, params);
      return (result.values ?? []).map((row) => row.value);
    };

    const promise = (async (): Promise<FilterValues> => {
      let values: string[] = [];
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

    this.valueCache.set(key, promise);
    return promise;
  }

  // ---- banner -----------------------------------------------------------

  setBanner(banner: Banner | null) {
    this.commit({ banner });
  }

  private handleError(err: unknown) {
    if (err instanceof ApiError && err.status === 401) {
      this.setBanner({
        tone: "warning",
        title: "This server requires an API token.",
        detail:
          "Run localStorage.setItem('lidlesseye.token', '…') in the console and reload.",
      });
      return;
    }
    if (err instanceof ApiError && err.status === 403) {
      this.setBanner({
        tone: "warning",
        title: err.message,
        detail:
          "Switch tenant with the Tenant selector, or clear lidlesseye.tenant in localStorage.",
      });
      return;
    }
    if (err instanceof ApiError && err.status === 429) {
      this.setBanner({ tone: "warning", title: err.message });
      return;
    }
    this.setBanner({
      tone: "error",
      title: "Request failed",
      detail: err instanceof Error ? err.message : String(err),
    });
  }

  // ---- loading ----------------------------------------------------------

  private queryParams() {
    const win = this.resolveWindow();
    const params: Record<string, string> = {
      profile_type: this.state.type,
      from: iso(win.from),
      until: iso(win.until),
    };
    const selector = composeSelector(this.state.filters);
    if (selector) params.selector = selector;
    if (this.state.groupBy && this.state.view !== "compare") {
      params.group_by = this.state.groupBy;
    }
    return params;
  }

  private diffParams() {
    const params = this.queryParams();
    const baseline = this.resolveBaseline();
    params.baseline_from = iso(baseline.from);
    params.baseline_until = iso(baseline.until);
    return params;
  }

  async reload() {
    const id = ++this.requestId;
    const params = this.queryParams();
    this.commit({ banner: null, loading: true, empty: null });

    try {
      const view = this.state.view;

      if (view === "overview") {
        const [stats, types, series] = await Promise.all([
          api.stats(),
          api.profileTypes(params),
          api.series(params),
        ]);
        if (id !== this.requestId) return;
        this.commit({
          overview: { stats, types: types.profile_types ?? [], series: series.series ?? [] },
        });
        return;
      }

      if (view === "flamegraph") {
        const result = await api.flamegraph(params);
        if (id !== this.requestId) return;
        if (!result.total) {
          this.commit({ flamegraph: null, empty: this.emptyFlamegraph() });
          return;
        }
        this.commit({ flamegraph: result });
        return;
      }

      if (view === "compare") {
        const result = await api.diff(this.diffParams());
        if (id !== this.requestId) return;
        if (!result.total_left && !result.total_right) {
          this.commit({
            flamegraph: null,
            empty: {
              title: "Nothing to compare",
              hint: "Neither window has samples for this profile and filter.",
            },
          });
          return;
        }
        this.commit({ flamegraph: result });
        return;
      }

      if (view === "top") {
        const result = await api.top(params);
        if (id !== this.requestId) return;
        if (!result.items.length) {
          this.commit({
            top: null,
            empty: {
              title: "Nothing in this range",
              hint: "No stacks matched the selected profile and filters.",
            },
          });
          return;
        }
        this.commit({ top: result });
        return;
      }

      if (view === "timeseries") {
        const unit = await this.unitFor(this.state.type, params);
        if (id !== this.requestId) return;
        const result = await api.timeseries(params);
        if (id !== this.requestId) return;
        this.commit({ series: result, unit });
        return;
      }

      if (view === "labels") {
        await this.loadLabels(id, params);
      }
    } catch (err) {
      if (id !== this.requestId) return;
      this.handleError(err);
    } finally {
      if (id === this.requestId) this.commit({ loading: false });
    }
  }

  private emptyFlamegraph(): EmptyStateInfo {
    const tenantFlag = this.state.tenant ? ` --tenant ${this.state.tenant}` : "";
    return {
      title: "No samples yet",
      hint: this.state.tenant
        ? `Tenant "${this.state.tenant}" has no data in this range. Widen the range, or switch tenant.`
        : "Start an agent on a machine with eBPF enabled, or point an existing one here.",
      command: `lidlesseye collect --server http://${window.location.host}${tenantFlag} --frequency 99`,
    };
  }

  private async unitFor(type: string, params: Record<string, string>) {
    try {
      const types = await api.profileTypes(params);
      const match = (types.profile_types ?? []).find((row) => row.name === type);
      return match?.unit ?? "nanoseconds";
    } catch {
      return "nanoseconds";
    }
  }

  private async loadLabels(id: number, params: Record<string, string>) {
    const names = await api.labelNames(params);
    if (id !== this.requestId) return;
    const list = names.label_names ?? [];
    const active =
      this.state.activeLabel && list.includes(this.state.activeLabel)
        ? this.state.activeLabel
        : (list.find((name) => name !== "service_name") ?? list[0] ?? null);
    this.commit({ explorerNames: list, activeLabel: active, explorerValues: [] });
    if (!active) return;
    const values = await api.labelValues(active, params);
    if (id !== this.requestId) return;
    this.commit({ explorerValues: values.values ?? [] });
  }

  /** Pick a label in the explorer and load its values for the current window. */
  async selectLabel(name: string) {
    if (!name || name === this.state.activeLabel) return;
    this.commit({ activeLabel: name, explorerValues: [] });
    const id = this.requestId;
    const params = this.queryParams();
    try {
      const values = await api.labelValues(name, params);
      if (id !== this.requestId) return;
      this.commit({ explorerValues: values.values ?? [] });
    } catch (err) {
      this.handleError(err);
    }
  }

  // ---- histogram --------------------------------------------------------

  /** Refresh the histogram over the domain; single-flighted. */
  async loadHistogram() {
    const token = ++this.histToken;
    const domain = this.resolveDomain();
    const selector = composeSelector(this.state.filters);
    const params: Record<string, string | number> = {
      profile_type: this.state.type,
      from: iso(domain.from),
      until: iso(domain.until),
      buckets: 120,
    };
    if (selector) params.selector = selector;
    try {
      const payload = await api.timeseries(params);
      if (token !== this.histToken) return;
      const empty = (payload.series ?? []).every((series) =>
        (series.points ?? []).every((point) => !point),
      );
      this.commit({ histData: payload, histEmpty: empty });
    } catch {
      // The histogram is decoration on top of a working query path; a failure
      // here (token expiry, transient 5xx) must not blank the views.
    }
  }

  async loadStats() {
    try {
      const stats = await api.stats();
      const store = stats?.store ?? {};
      this.commit({
        dataRange:
          store.earliest_ns && store.latest_ns
            ? { from: store.earliest_ns, until: store.latest_ns }
            : null,
      });
    } catch {
      this.commit({ dataRange: null });
    }
  }

  /** "No samples here" escape hatch: jump to the range that has data. */
  useDataRange() {
    const range = this.state.dataRange;
    if (!range) return;
    this.setAbsoluteDomain(range.from, range.until);
  }

  // ---- populating the toolbar -------------------------------------------

  private async loadTenants() {
    try {
      const result = await api.tenants();
      const list = (result.tenants ?? []).map((row) => row.id);
      if (!list.length) list.push("default");
      if (this.state.tenant && !list.includes(this.state.tenant)) list.push(this.state.tenant);
      const tenant = this.state.tenant || (list[0] ?? "default");
      // The request layer reads the tenant from storage, so choosing one here
      // has to be persisted too. Without this the first query went to the
      // server's default tenant while the selector already displayed another —
      // the empty state then blamed the wrong tenant.
      setTenant(tenant);
      this.commit({ tenants: list, tenant });
    } catch (err) {
      // A tenant-scoped token cannot enumerate tenants; that is fine.
      if (!(err instanceof ApiError && (err.status === 403 || err.status === 401))) {
        this.handleError(err);
      }
      this.commit({ tenants: [], tenant: this.state.tenant || "default" });
    }
  }

  private async loadProfileTypes() {
    try {
      const win = this.resolveWindow();
      const result = await api.profileTypes({ from: iso(win.from), until: iso(win.until) });
      const types = (result.profile_types ?? []).map((row) => row.name);
      if (!types.includes("cpu")) types.unshift("cpu");
      const type = types.includes(this.state.type) ? this.state.type : (types[0] ?? "cpu");
      this.commit({ profileTypes: types, type });
    } catch (err) {
      this.handleError(err);
    }
  }

  private async loadLabelNames() {
    try {
      const result = await api.labelNames({ from: "now-7d", until: "now" });
      const names = (result.label_names ?? []).filter((name) => name !== "service_name");
      this.commit({ labelNames: names });
    } catch {
      /* grouping is optional */
    }
  }
}

/** One store per page: the URL is the only other source of truth. */
export const dashboardStore = new DashboardStore();

export { parseSelector };
