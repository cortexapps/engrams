import { type ReactNode, useState } from "react";
import { useSessionCheckpoints } from "../hooks/useCheckpoints";
import { useHosts } from "../hooks/useHosts";
import { useTeleportSession } from "../hooks/useTeleportSession";
import { usePauseResumeSession } from "../hooks/usePauseResumeSession";
import { useIsAdmin } from "../auth/AuthProvider";

import { relativeTime } from "@/lib/relative-time";
import { DurabilityTimeline } from "./DurabilityTimeline";
import { MetricRow } from "./MetricRow";
import { Button } from "@/components/ui/button";
import { StatusDot } from "@/components/status-dot";
import { Text } from "@/components/ui/text";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type { CheckpointSummary, IndexedEvent, Session, SessionState } from "../lib/types";

// The session's operator/forensic surface, kept OUT of the developer's way.
// A standing right rail spent the work surface's width, all session, on data
// the 90%-case developer never reads (the recovery ladder, admin live-ops). It
// splits into two:
//
//   - DurabilityReadout — the only durability the developer sees inline: a
//     calm "is my work safe" telltale (glyph + word), driven by
//     useDurabilitySummary. Honest about every lifecycle state; silent rather
//     than alarming when there is no checkpoint yet.
//   - DiagnosticsPanel — a view inside the work pane: the checkpoint ladder +
//     the image/created/events facts + the raw event stream, with the
//     admin-only teleport / pause-resume controls gated INSIDE (a non-admin
//     sees a read-only durability ledger). It sits beside Shell +
//     Browser in the same resizable pane rather than in its own drawer.

// ---------------------------------------------------------------------------
// Durability telltale
// ---------------------------------------------------------------------------

export type DurabilitySummary = {
  /** Instrument vocabulary only — never the lime accent. */
  tone: "nominal" | "caution";
  /** Carries the meaning on its own (colour is reinforcement, never the sole
   * signal). */
  label: string;
  /** The forensic detail, for the hover title. */
  title: string;
};

/** Reduce the checkpoint chain to a one-line, developer-grade reassurance.
 * Returns null whenever there's nothing honest + calming to say — the header
 * simply shows no telltale rather than operator noise or alarm. Exported for
 * the unit test that pins this honest-state contract. */
export function durabilitySummary(
  status: SessionState,
  checkpoints: CheckpointSummary[],
): DurabilitySummary | null {
  // Terminal — the status glyph already says completed/failed/dead. Stay silent.
  if (status === "completed" || status === "failed" || status === "dead") return null;

  const recoverable = checkpoints.find((c) => c.recoverable) ?? null;

  // Suspended / transitional: durability rides the latest snapshot. Reassure
  // only when a recoverable point exists; never alarm.
  if (
    status === "idle" ||
    status === "host_lost" ||
    status === "evicting" ||
    status === "evacuating"
  ) {
    if (recoverable) {
      return {
        tone: "nominal",
        label: "Safe · recoverable",
        title: `Recoverable from a checkpoint ${relativeTime(recoverable.created_at)}`,
      };
    }
    return null;
  }

  // Active-ish (active / created / pending / queued): the newest checkpoint
  // is the durable point. Before the first one there is nothing honest to
  // claim — stay quiet.
  const latest = checkpoints[0];
  if (!latest) return null;
  const n = checkpoints.length;
  return {
    tone: "nominal",
    label: `Work saved · ${relativeTime(latest.created_at)}`,
    title: `Last checkpoint ${relativeTime(latest.created_at)} · ${n} checkpoint${n === 1 ? "" : "s"}`,
  };
}

/** Owns the checkpoint poll once and derives the header telltale. Shared by
 * React Query with the drawer's ladder, so there's a single poll per session. */
export function useDurabilitySummary(
  sessionId: string,
  status: SessionState | undefined,
): DurabilitySummary | null {
  const ckpt = useSessionCheckpoints(sessionId);
  if (!status) return null;
  return durabilitySummary(status, ckpt.data?.checkpoints ?? []);
}

/** The inline header telltale: a tone-coloured dot + the plain-language word,
 * the forensic detail one hover/focus away in a Tooltip (same disclosure the
 * adjacent ProfileChip uses). Colour is reinforcement; the word stands alone
 * (PRODUCT.md status grammar). Carries its own provider so it's safe anywhere. */
export function DurabilityReadout({ summary }: { summary: DurabilitySummary }) {
  return (
    <TooltipProvider delayDuration={100}>
      <Tooltip>
        <TooltipTrigger asChild>
          <span className="inline-flex items-center gap-1.5 text-muted-foreground">
            <StatusDot tone={summary.tone} size={6} />
            {summary.label}
          </span>
        </TooltipTrigger>
        <TooltipContent align="start">{summary.title}</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}

// ---------------------------------------------------------------------------
// Diagnostics drawer
// ---------------------------------------------------------------------------

function SectionLabel({ children }: { children: ReactNode }) {
  return <h2 className="mb-2.5 text-sm font-semibold">{children}</h2>;
}

export function DiagnosticsPanel({
  session,
  sessionId,
  events,
}: {
  session: Session | undefined;
  sessionId: string;
  events: IndexedEvent[];
}) {
  const eventCount = events.length;
  return (
    // Two views: the calm operator readout, and the raw append-only event
    // stream (moved off the transcript's top tabs — the forensic surface lives
    // with the rest of the diagnostics).
    <Tabs defaultValue="overview" className="flex h-full min-h-0 flex-col gap-0">
      <div className="shrink-0 border-b px-4 pt-2.5">
        <TabsList variant="line">
          <TabsTrigger value="overview">Overview</TabsTrigger>
          <TabsTrigger value="raw" data-testid="diagnostics-tab-raw">
            Raw events
          </TabsTrigger>
        </TabsList>
      </div>

      <TabsContent value="overview" className="min-h-0 space-y-6 overflow-y-auto px-4 py-5">
        {/* "Show the receipts": the durability detail, in-context beside the
            session it describes. Durability is keyed by id, so it renders even
            before the session row resolves. */}
        <section>
          <SectionLabel>Durability</SectionLabel>
          <DurabilityTimeline sessionId={sessionId} />
        </section>

        {session && (
          <section className="border-t pt-4">
            <SectionLabel>Session</SectionLabel>
            <dl className="space-y-2.5 text-sm">
              <div>
                <dt className="text-muted-foreground">Image</dt>
                <dd className="mt-0.5 font-mono text-sm break-all text-foreground">
                  {session.image}
                </dd>
              </div>
              <MetricRow label="Created" value={relativeTime(session.created_at)} />
              {/* The count keeps its own element: any poller reads
                  Number(textContent) of exactly this span. */}
              <MetricRow
                label="Events"
                value={<span data-testid="event-count">{eventCount}</span>}
              />
            </dl>
          </section>
        )}

        {/* Admin live-ops — self-gating (admin + Active only), so a non-admin
            sees a clean read-only ledger above and nothing here. */}
        {session && <TeleportControl session={session} events={events} />}
        {session && <PauseResumeControl session={session} />}
      </TabsContent>

      <TabsContent value="raw" className="min-h-0 overflow-hidden">
        <RawEvents events={events} />
      </TabsContent>
    </Tabs>
  );
}

// The raw append-only event stream — idx · type · a compact one-liner of the
// body. Forensic detail, so it lives in the Diagnostics drawer rather than the
// transcript's chrome. Rendered only when its tab is active (Radix unmounts
// inactive content), so the list isn't built until asked for.
function RawEvents({ events }: { events: IndexedEvent[] }) {
  return (
    <div className="h-full space-y-0.5 overflow-auto px-5 py-4 font-mono text-xs text-muted-foreground">
      {events.map((e) => (
        <div
          key={e.idx}
          data-testid="event-row"
          className="grid items-baseline gap-3"
          style={{ gridTemplateColumns: "4ch min-content 1fr" }}
        >
          <span className="tabular-nums text-muted-foreground/70">{e.idx}</span>
          {/* An event type is a wire identifier, so it reads as machine data. */}
          <span className="font-mono text-2xs">{e.event.type}</span>
          <span className="truncate text-foreground/80" title={JSON.stringify(e.event)}>
            {summarizeRaw(e.event)}
          </span>
        </div>
      ))}
    </div>
  );
}

function summarizeRaw(ev: unknown): string {
  // Compact one-liner of any event body — used only in the raw log.
  const obj = ev as Record<string, unknown>;
  return Object.entries(obj)
    .filter(([k]) => k !== "type" && k !== "at")
    .map(([k, v]) => `${k}=${JSON.stringify(v)}`)
    .join(" ");
}

// ---------------------------------------------------------------------------
// Admin live-ops (ADR 0045 Phase F) — relocated from the session-detail rail.
// ---------------------------------------------------------------------------

// Admission returns the move identity; the durable event marks its outcome.
function TeleportControl({ session, events }: { session: Session; events: IndexedEvent[] }) {
  const isAdmin = useIsAdmin();
  const { data: hosts } = useHosts();
  const teleport = useTeleportSession(session.id);
  const [target, setTarget] = useState<string>("");

  const finished = events
    .map((item) => item.event)
    .reverse()
    .find(
      (event) =>
        event.type === "teleport_finished" && event.teleport_id === teleport.data?.teleportId,
    );
  if (!isAdmin || (session.status !== "active" && !teleport.data)) return null;

  const candidates = (hosts ?? []).filter((h) => h.status === "ready" && h.id !== session.host_id);

  return (
    <div className="border-t pt-4">
      <SectionLabel>Teleport</SectionLabel>
      {teleport.error && (
        <Text tone="muted" className="text-xs">
          {teleport.error.message}
        </Text>
      )}
      {teleport.data && (
        <Text tone="muted" className="text-xs">
          {teleport.data.kind} to {teleport.data.destHostId}:{" "}
          {finished?.type === "teleport_finished" ? finished.outcome : "in progress"}
          {finished?.type === "teleport_finished" && finished.error ? ` — ${finished.error}` : ""}
        </Text>
      )}
      {candidates.length === 0 ? (
        <Text tone="muted" className="text-xs">
          No other ready host is available.
        </Text>
      ) : (
        <div className="space-y-2">
          <Select value={target} onValueChange={setTarget}>
            <SelectTrigger className="h-8 text-xs">
              <SelectValue placeholder="Destination host…" />
            </SelectTrigger>
            <SelectContent>
              {candidates.map((h) => {
                const freeGib = ((h.capacity_total_mib - h.capacity_used_mib) / 1024).toFixed(1);
                const label = h.hostname || h.id.slice(0, 8);
                return (
                  <SelectItem key={h.id} value={h.id} className="text-xs">
                    {label} · {freeGib} GiB free · {h.running_sandboxes} vm
                  </SelectItem>
                );
              })}
            </SelectContent>
          </Select>
          <Button
            size="sm"
            variant="secondary"
            className="w-full"
            disabled={
              !target ||
              teleport.isPending ||
              session.status !== "active" ||
              (!!teleport.data && !finished)
            }
            onClick={() => teleport.mutate(target)}
          >
            {teleport.isPending ? "Teleporting…" : "Teleport"}
          </Button>
        </div>
      )}
    </div>
  );
}

// Freeze / unfreeze this session's microVM in place — the admin affordance to
// drive + observe the pause/resume path. Does not change session state (the row
// stays `active`), so both buttons are always offered; the operator picks.
// Admin-only, Active-only.
function PauseResumeControl({ session }: { session: Session }) {
  const isAdmin = useIsAdmin();
  const { pause, resume } = usePauseResumeSession(session.id);

  if (!isAdmin || session.status !== "active") return null;

  return (
    <div className="border-t pt-4">
      <SectionLabel>Freeze</SectionLabel>
      <div className="flex gap-2">
        <Button
          size="sm"
          variant="secondary"
          className="flex-1"
          disabled={pause.isPending}
          onClick={() => pause.mutate()}
        >
          {pause.isPending ? "Pausing…" : "Pause"}
        </Button>
        <Button
          size="sm"
          variant="secondary"
          className="flex-1"
          disabled={resume.isPending}
          onClick={() => resume.mutate()}
        >
          {resume.isPending ? "Resuming…" : "Resume"}
        </Button>
      </div>
    </div>
  );
}
