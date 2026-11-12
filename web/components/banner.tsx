"use client";

import { CircleAlert, TriangleAlert } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { useDashboard } from "@/lib/use-dashboard";

/**
 * The error surface. The dashboard reports failures as a banner instead of a
 * modal because the common cases (expired token, wrong tenant, rate limit) are
 * fixed by changing a control that the banner sits next to.
 */
export function Banner() {
  const { banner } = useDashboard();
  if (!banner) return null;

  const isError = banner.tone === "error";
  const Icon = isError ? CircleAlert : TriangleAlert;

  return (
    <div className="px-3 pt-3">
      <Alert variant={isError ? "destructive" : "default"} data-testid="banner">
        <Icon />
        <AlertTitle>{banner.title}</AlertTitle>
        {banner.detail ? <AlertDescription>{banner.detail}</AlertDescription> : null}
      </Alert>
    </div>
  );
}
