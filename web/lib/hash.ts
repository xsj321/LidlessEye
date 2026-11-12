// The URL is the source of truth for a view.
//
// Every knob that changes what you see lives in the hash (`#/view?a=b`), so a
// pasted link reproduces the same screen. `?tenant=` in the query string is
// also honoured because that is the form people bookmark before opening the
// dashboard for the first time.

import { composeSelector, parseSelector } from "./selector";
import type { LabelFilter, View } from "./types";
import { VIEWS } from "./types";
import { resolveBaseline, resolveDomain, resolveWindow } from "./time";
import type { RangeMode, TimeState } from "./time";

export interface HashPatch {
  view?: View;
  type?: string;
  mode?: RangeMode;
  range?: string;
  domainFrom?: number;
  domainUntil?: number;
  fromNs?: number;
  untilNs?: number;
  baseFromNs?: number;
  baseUntilNs?: number;
  filters?: LabelFilter[];
  groupBy?: string;
  tenant?: string;
}

export interface HashState extends TimeState {
  view: View;
  type: string;
  filters: LabelFilter[];
  groupBy: string;
  tenant: string;
}

export function parseHash(hash: string): HashPatch {
  const raw = hash.replace(/^#\/?/, "");
  const [view, query] = raw.split("?");
  const params = new URLSearchParams(query ?? "");
  const patch: HashPatch = {};

  if (VIEWS.includes(view as View)) patch.view = view as View;
  if (params.get("type")) patch.type = params.get("type") as string;

  const from = Number(params.get("from"));
  const until = Number(params.get("until"));
  if (Number.isFinite(from) && Number.isFinite(until) && until > from) {
    patch.mode = "absolute";
    patch.domainFrom = from;
    patch.domainUntil = until;
  } else if (params.get("range")) {
    patch.mode = "relative";
    patch.range = params.get("range") as string;
  }

  const wfrom = Number(params.get("wfrom"));
  const wuntil = Number(params.get("wuntil"));
  if (Number.isFinite(wfrom) && Number.isFinite(wuntil) && wuntil > wfrom) {
    patch.fromNs = wfrom;
    patch.untilNs = wuntil;
  } else {
    patch.fromNs = 0;
    patch.untilNs = 0;
  }

  const bfrom = Number(params.get("bwfrom"));
  const buntil = Number(params.get("bwuntil"));
  if (Number.isFinite(bfrom) && Number.isFinite(buntil) && buntil > bfrom) {
    patch.baseFromNs = bfrom;
    patch.baseUntilNs = buntil;
  }

  if (params.has("selector")) patch.filters = parseSelector(params.get("selector"));
  if (params.has("group_by")) patch.groupBy = params.get("group_by") as string;
  if (params.has("tenant")) patch.tenant = params.get("tenant") as string;

  return patch;
}

/** `#/view?type=…` for the current state. */
export function buildHash(state: HashState): string {
  const params = new URLSearchParams();
  params.set("type", state.type);
  const domain = resolveDomain(state);
  if (state.mode === "absolute") {
    params.set("from", String(state.domainFrom));
    params.set("until", String(state.domainUntil));
  } else {
    params.set("range", state.range);
  }
  const win = resolveWindow(state);
  const whole =
    Math.abs(win.from - domain.from) < 1e6 && Math.abs(win.until - domain.until) < 1e6;
  if (!whole) {
    params.set("wfrom", String(win.from));
    params.set("wuntil", String(win.until));
  }
  if (state.view === "compare") {
    const baseline = resolveBaseline(state);
    params.set("bwfrom", String(baseline.from));
    params.set("bwuntil", String(baseline.until));
  }
  const selector = composeSelector(state.filters);
  if (selector) params.set("selector", selector);
  if (state.groupBy) params.set("group_by", state.groupBy);
  if (state.tenant) params.set("tenant", state.tenant);
  return `#/${state.view}?${params.toString()}`;
}

/** Replace the URL without adding a history entry (the view state owns it). */
export function syncHash(state: HashState): void {
  const next = buildHash(state);
  if (window.location.hash !== next) history.replaceState(null, "", next);
}
