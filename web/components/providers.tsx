"use client";

import { useEffect } from "react";

import { TooltipProvider } from "@/components/ui/tooltip";
import { dashboardStore } from "@/lib/store";
import type { View } from "@/lib/types";

/**
 * The console/debug handle the dashboard has always exposed:
 * `window.lidlesseye.state.mode` in a console, and the same object in the
 * browser smoke test. `state` is a getter so it is always the live snapshot.
 */
function installDebugHandle() {
  const handle = {
    get state() {
      return dashboardStore.getSnapshot();
    },
    reload: () => dashboardStore.reload(),
    setView: (view: View) => dashboardStore.setView(view),
    switchTenant: (tenant: string) => dashboardStore.setTenantId(tenant),
    setAbsoluteDomain: (from: number, until: number) =>
      dashboardStore.setAbsoluteDomain(from, until),
    setRelativeRange: (preset: string) => dashboardStore.setRelativeRange(preset),
  };
  (window as unknown as { lidlesseye: typeof handle }).lidlesseye = handle;
}

export function Providers({ children }: { children: React.ReactNode }) {
  // Booting in an effect (not during render) keeps the static prerender and
  // the first client render identical: both see the empty initial state, and
  // the first fetch starts after hydration.
  useEffect(() => {
    installDebugHandle();
    dashboardStore.initialize();
  }, []);

  return <TooltipProvider delayDuration={300}>{children}</TooltipProvider>;
}
