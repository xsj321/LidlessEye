"use client";

import { useSyncExternalStore } from "react";

import { dashboardStore } from "./store";
import type { DashboardState } from "./store";

/** Subscribe to the dashboard store. Returns the whole state object. */
export function useDashboard(): DashboardState {
  return useSyncExternalStore(
    dashboardStore.subscribe,
    dashboardStore.getSnapshot,
    dashboardStore.getServerSnapshot,
  );
}
