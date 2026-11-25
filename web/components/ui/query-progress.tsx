"use client";

import { useEffect, useState } from "react";

import { useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";

/**
 * Shown while a query is in flight.
 *
 * A blank panel is indistinguishable from "this tenant has no data", and a
 * query can take seconds — a cold Parquet archive, a busy node, a wide window.
 * The elapsed counter is what tells the two apart, and it keeps climbing while
 * the server is still working, so a slow answer never looks like a missing one.
 *
 * The ring is indeterminate because the server reports no progress. Under
 * `prefers-reduced-motion` it stops spinning; the counter is the real signal.
 */
export function QueryProgress({
  height = 420,
  label,
  className,
}: {
  /** Kept adjacent to the skeleton's height so the swap does not shift layout. */
  height?: number;
  label?: string;
  className?: string;
}) {
  const t = useT();
  const [elapsed, setElapsed] = useState(0);

  useEffect(() => {
    const started = performance.now();
    const timer = window.setInterval(() => {
      setElapsed((performance.now() - started) / 1000);
    }, 100);
    return () => window.clearInterval(timer);
  }, []);

  return (
    <div
      role="status"
      aria-live="polite"
      className={cn(
        "flex w-full flex-col items-center justify-center gap-4 rounded-lg border border-dashed border-border/60 bg-accent/20",
        className,
      )}
      style={{ height }}
    >
      <svg
        viewBox="0 0 40 40"
        className="size-10 motion-safe:animate-spin"
        aria-hidden="true"
      >
        <circle
          cx="20"
          cy="20"
          r="16"
          fill="none"
          stroke="currentColor"
          strokeWidth="3"
          className="text-border"
        />
        <circle
          cx="20"
          cy="20"
          r="16"
          fill="none"
          stroke="currentColor"
          strokeWidth="3"
          strokeLinecap="round"
          strokeDasharray="30 70"
          className="text-muted-foreground"
        />
      </svg>
      <p className="text-sm text-muted-foreground">{label ?? t("Querying…")}</p>
      {/* Hidden from assistive tech: `role="status"` would otherwise announce
          the counter ten times a second. */}
      <p
        className="font-mono text-xs text-muted-foreground/70 tabular-nums"
        aria-hidden="true"
      >
        {t("Elapsed")} {elapsed.toFixed(1)}s
      </p>
    </div>
  );
}
