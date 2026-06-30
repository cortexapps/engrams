/**
 * Live-host port exposures (ADR 0064) — a compact section for the Diagnostics
 * drawer: a liveness dot + an external link per exposed guest port, plus a
 * minimal "expose a port" control.
 *
 * INTERIM HOME. This is deliberately *not* a first-class surface yet — it lives
 * in the operator drawer, out of the developer's way, until ADR 0065 lands the
 * live agent browser. At that point the BROWSER tab absorbs THIS component
 * verbatim as its "exposed ports" rail beside the live screencast; nothing here
 * is throwaway, it just relocates. (See ADR 0065 §web and the DiagnosticsDrawer
 * header note.)
 */

import { useState } from "react";
import {
  usePorts,
  usePortHealth,
  useExposePort,
  useRevokePort,
  shareUrl,
  type PortExposure,
  type PortHealth,
} from "../../hooks/usePorts";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";

/** Liveness → the drawer's instrument vocabulary (same grammar as
 * DurabilityReadout: the tooltip word carries the meaning, colour is
 * reinforcement). `unknown` is a hollow ring — honest silence when the session
 * isn't running, never a red "down" guess. */
function livenessDot(health: PortHealth | undefined): { color: string; title: string } {
  switch (health) {
    case "up":
      return { color: "var(--color-instrument-nominal)", title: "serving" };
    case "down":
      return { color: "var(--color-instrument-caution)", title: "no response on this port" };
    default:
      return { color: "transparent", title: "unknown — session not running" };
  }
}

function ExposureRow({
  sessionId,
  exposure,
  active,
  onRevoke,
  revoking,
}: {
  sessionId: string;
  exposure: PortExposure;
  active: boolean;
  onRevoke: () => void;
  revoking: boolean;
}) {
  // Only probe (and thus poll) while the session is active — the gate that keeps
  // a liveness check from ever waking a suspended VM.
  const { data: health } = usePortHealth(sessionId, exposure.slug, active);
  const dot = livenessDot(active ? health : undefined);
  const link = shareUrl(exposure);

  return (
    <div className="flex items-center justify-between gap-2 py-1.5 text-sm">
      <div className="flex min-w-0 items-center gap-2">
        <TooltipProvider delayDuration={100}>
          <Tooltip>
            <TooltipTrigger asChild>
              <span
                aria-hidden
                className="size-1.5 shrink-0 rounded-full ring-1 ring-border"
                style={{ backgroundColor: dot.color }}
                data-testid={`liveness-${exposure.slug}`}
                data-health={active ? (health ?? "unknown") : "unknown"}
              />
            </TooltipTrigger>
            <TooltipContent align="start">{dot.title}</TooltipContent>
          </Tooltip>
        </TooltipProvider>
        <a
          href={link}
          target="_blank"
          rel="noreferrer"
          className="truncate font-mono text-xs text-primary hover:underline"
          data-testid={`open-${exposure.slug}`}
        >
          :{exposure.port}
          {exposure.label ? ` · ${exposure.label}` : ""} ↗
        </a>
      </div>
      <Button
        size="sm"
        variant="ghost"
        className="h-6 shrink-0 px-2 text-xs"
        onClick={onRevoke}
        disabled={revoking}
        data-testid={`revoke-${exposure.slug}`}
      >
        Revoke
      </Button>
    </div>
  );
}

export function ExposedPortsSection({
  sessionId,
  active,
}: {
  sessionId: string;
  /** Session is running — gates the liveness probe (no probe → no resume). */
  active: boolean;
}) {
  const { data: exposures } = usePorts(sessionId);
  const expose = useExposePort(sessionId);
  const revoke = useRevokePort(sessionId);
  const [port, setPort] = useState("");
  const [error, setError] = useState<string | null>(null);

  const onExpose = () => {
    const p = Number(port);
    if (!Number.isInteger(p) || p < 1 || p > 65535) {
      setError("Enter a port between 1 and 65535");
      return;
    }
    setError(null);
    expose.mutate(
      { port: p },
      {
        onSuccess: () => setPort(""),
        onError: (e) => {
          // The unique (session, port) index 409s a re-expose; say so plainly
          // rather than surfacing the raw status.
          const msg = String(e);
          setError(/409/.test(msg) ? `Port ${p} is already exposed` : msg);
        },
      },
    );
  };

  return (
    <div>
      <div className="flex items-center gap-2">
        <Input
          type="number"
          min={1}
          max={65535}
          placeholder="port (e.g. 3000)"
          value={port}
          onChange={(e) => setPort(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onExpose();
          }}
          className="h-7 w-32 text-xs"
          data-testid="port-input"
        />
        <Button
          size="sm"
          variant="secondary"
          className="h-7 text-xs"
          onClick={onExpose}
          disabled={expose.isPending}
          data-testid="expose-btn"
        >
          {expose.isPending ? "Exposing…" : "Expose"}
        </Button>
      </div>
      {error && (
        <p className="mt-1.5 text-xs text-destructive" data-testid="ports-error">
          {error}
        </p>
      )}
      <div className="mt-2 divide-y">
        {exposures && exposures.length === 0 && (
          <p className="py-1.5 text-xs text-muted-foreground" data-testid="ports-empty">
            No ports exposed.
          </p>
        )}
        {exposures?.map((e) => (
          <ExposureRow
            key={e.slug}
            sessionId={sessionId}
            exposure={e}
            active={active}
            onRevoke={() => revoke.mutate(e.slug)}
            revoking={revoke.isPending}
          />
        ))}
      </div>
    </div>
  );
}
