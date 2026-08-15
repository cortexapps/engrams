/**
 * Session apps (ADR 0118) — a compact section for the Diagnostics drawer: a
 * liveness dot + an external link per app, plus a minimal "publish a port"
 * control.
 *
 * An app added here gets no environment variable: the guest's environment is
 * fixed when the harness binds, so a process that is already running cannot be
 * told about a name that did not exist when it started. Declare the app on the
 * profile if a sibling has to reach it.
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
  useApps,
  useAppHealth,
  useReserveApp,
  useRevokeApp,
  type SessionApp,
  type AppHealth,
} from "../../hooks/useApps";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";

/** Liveness → the drawer's instrument vocabulary (same grammar as
 * DurabilityReadout: the tooltip word carries the meaning, colour is
 * reinforcement). `unknown` is a hollow ring — honest silence when the session
 * isn't running, never a red "down" guess. */
function livenessDot(health: AppHealth | undefined): { color: string; title: string } {
  switch (health) {
    case "up":
      return { color: "var(--color-instrument-nominal)", title: "serving" };
    case "down":
      return { color: "var(--color-instrument-caution)", title: "no response on this port" };
    default:
      return { color: "transparent", title: "unknown — session not running" };
  }
}

function AppRow({
  sessionId,
  app,
  active,
  onRevoke,
  revoking,
}: {
  sessionId: string;
  app: SessionApp;
  active: boolean;
  onRevoke: () => void;
  revoking: boolean;
}) {
  // Only probe (and thus poll) while the session is active — the gate that keeps
  // a liveness check from ever waking a suspended VM.
  const { data: health } = useAppHealth(sessionId, app.hostLabel, active);
  const dot = livenessDot(active ? health : undefined);

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
                data-testid={`liveness-${app.hostLabel}`}
                data-health={active ? (health ?? "unknown") : "unknown"}
              />
            </TooltipTrigger>
            <TooltipContent align="start">{dot.title}</TooltipContent>
          </Tooltip>
        </TooltipProvider>
        <a
          href={app.url}
          target="_blank"
          rel="noreferrer"
          className="truncate font-mono text-xs text-primary hover:underline"
          data-testid={`open-${app.hostLabel}`}
        >
          {app.name} :{app.port} ↗
        </a>
      </div>
      <Button
        size="sm"
        variant="ghost"
        className="h-6 shrink-0 px-2 text-xs"
        onClick={onRevoke}
        disabled={revoking}
        data-testid={`revoke-${app.hostLabel}`}
      >
        Revoke
      </Button>
    </div>
  );
}

export function SessionAppsSection({
  sessionId,
  active,
}: {
  sessionId: string;
  /** Session is running — gates the liveness probe (no probe → no resume). */
  active: boolean;
}) {
  const { data: apps } = useApps(sessionId);
  const reserve = useReserveApp(sessionId);
  const revoke = useRevokeApp(sessionId);
  const [port, setPort] = useState("");
  const [error, setError] = useState<string | null>(null);

  const onExpose = () => {
    const p = Number(port);
    if (!Number.isInteger(p) || p < 1 || p > 65535) {
      setError("Enter a port between 1 and 65535");
      return;
    }
    setError(null);
    reserve.mutate(
      { port: p },
      {
        onSuccess: () => setPort(""),
        onError: (e) => {
          // The unique (session, port) index 409s a re-reserve; say so plainly
          // rather than surfacing the raw status.
          const msg = String(e);
          setError(/409/.test(msg) ? `Port ${p} is already published` : msg);
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
          disabled={reserve.isPending}
          data-testid="expose-btn"
        >
          {reserve.isPending ? "Publishing…" : "Publish"}
        </Button>
      </div>
      {error && (
        <p className="mt-1.5 text-xs text-destructive" data-testid="ports-error">
          {error}
        </p>
      )}
      <div className="mt-2 divide-y">
        {apps && apps.length === 0 && (
          <p className="py-1.5 text-xs text-muted-foreground" data-testid="ports-empty">
            No apps published.
          </p>
        )}
        {apps?.map((a) => (
          <AppRow
            key={a.hostLabel}
            sessionId={sessionId}
            app={a}
            active={active}
            onRevoke={() => revoke.mutate(a.hostLabel)}
            revoking={revoke.isPending}
          />
        ))}
      </div>
    </div>
  );
}
