"use client";

import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import type { EmptyStateInfo } from "@/lib/store";

/** Placeholder shown when a query returns nothing. */
export function EmptyState({ info }: { info: EmptyStateInfo }) {
  return (
    <div className="flex min-h-[320px] items-center justify-center p-6">
      <Card className="w-full max-w-xl border-dashed bg-transparent shadow-none">
        <CardHeader>
          <CardTitle className="text-base">{info.title}</CardTitle>
          <CardDescription>{info.hint}</CardDescription>
        </CardHeader>
        {info.command ? (
          <CardContent>
            <pre className="overflow-x-auto rounded-md border bg-secondary/60 px-3 py-2 font-mono text-xs text-muted-foreground">
              {info.command}
            </pre>
          </CardContent>
        ) : null}
      </Card>
    </div>
  );
}
