import { type ReactNode, useState } from "react";
import { Activity } from "lucide-react";
import { useSessionCowState } from "../hooks/useCowState";
import { useSessionCheckpoints } from "../hooks/useCheckpoints";
import { useHosts } from "../hooks/useHosts";
import { useTeleportSession } from "../hooks/useTeleportSession";
import { usePauseResumeSession } from "../hooks/usePauseResumeSession";
import { useIsAdmin } from "../auth/AuthProvider";
import { fmtAgo } from "../format";
import { relativeTime } from "../pages/sessions/session-format";
import { SessionCowState } from "./CowState";
import { DurabilityTimeline } from "./DurabilityTimeline";
import { MetricRow } from "./MetricRow";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from "@/components/ui/sheet";
import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { CheckpointSummary, CowStateView, Session, SessionState } from "../lib/types";

// The session's operator/forensic surface, kept OUT of the developer's way.
// A standing right rail spent the work surface's width, all session, on data
// the 90%-case developer never reads (chunk-level COW telemetry, the recovery
// ladder, admin live-ops). It now lives behind one button:
//
//   - DurabilityReadout — the only durability the developer sees inline: a
//     calm "is my work safe" telltale (glyph + word), driven by
//     useDurabilitySummary. Honest about every lifecycle state; silent rather
//     than alarming when telemetry is absent.
//   - DiagnosticsDrawer — on demand, for everyone: the full COW gauges + the
//     checkpoint ladder + the image/created/events facts, with the admin-only
//     teleport / pause-resume controls gated INSIDE (a non-admin's drawer is a
//     read-only durability ledger). Full-width on mobile, so phones finally see
//     this data (the old rail was desktop-only). (ADR 0052 follow-up)

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

/** The most recent of several ISO timestamps, or null if all are absent. */
function mostRecentIso(...isos: (string | null | undefined)[]): string | null {
  let best: string | null = null;
  let bestT = -Infinity;
  for (const iso of isos) {
    if (!iso) continue;
    const t = new Date(iso).getTime();
    if (Number.isFinite(t) && t > bestT) {
      bestT = t;
      best = iso;
    }
  }
  return best;
}

/** Reduce the raw durability telemetry to a one-line, developer-grade
 * reassurance. Returns null whenever there's nothing honest + calming to say —
 * the header simply shows no telltale rather than operator noise or alarm.
 * Exported for the unit test that pins this honest-state contract. */
export function durabilitySummary(
  status: SessionState,
  state: CowStateView | null,
  checkpoints: CheckpointSummary[],
): DurabilitySummary | null {
  // Terminal — the status glyph already says completed/failed/dead. Stay silent.
  if (status === "completed" || status === "failed" || status === "dead") return null;

  const recoverable = checkpoints.find((c) => c.recoverable) ?? null;

  // Suspended / transitional: there's no live disk tier; durability rides the
  // latest snapshot. Reassure only when a recoverable point exists; never alarm.
  if (
    status === "idle" ||
    status === "host_lost" ||
    status === "evicting" ||
    status === "evacuating"
  ) {
    if (recoverable) {
      return {
        tone: "nominal",
        label: "safe · recoverable",
        title: `recoverable from a checkpoint ${fmtAgo(recoverable.created_at)}`,
      };
    }
    return null;
  }

  // Active-ish (active / created / guest_ready / pending / queued). Without live
  // COW telemetry there's nothing honest to claim, and "unavailable" is operator
  // noise — stay quiet; the drawer explains the why.
  if (!state) return null;

  const anchor = mostRecentIso(
    state.last_flush_at,
    state.last_snapshot_at,
    checkpoints[0]?.created_at,
  );
  if (anchor) {
    const n = checkpoints.length;
    return {
      tone: "nominal",
      label: `work saved · ${fmtAgo(anchor)}`,
      title: `last flush ${fmtAgo(state.last_flush_at)} · last snapshot ${fmtAgo(
        state.last_snapshot_at,
      )} · ${n} checkpoint${n === 1 ? "" : "s"}`,
    };
  }
  // Dirty but nothing durable yet — in flight, not lost. Caution, calm.
  if (state.dirty_chunks > 0) {
    return {
      tone: "caution",
      label: "saving…",
      title: "changes not yet flushed to durable storage",
    };
  }
  // A clean slate with nothing written yet.
  return null;
}

/** Owns the two polling queries once and derives the header telltale. Shared by
 * React Query with the drawer's gauges, so there's a single poll per session. */
export function useDurabilitySummary(
  sessionId: string,
  status: SessionState | undefined,
): DurabilitySummary | null {
  const cow = useSessionCowState(sessionId);
  const ckpt = useSessionCheckpoints(sessionId);
  if (!status) return null;
  return durabilitySummary(status, cow.data?.state ?? null, ckpt.data?.checkpoints ?? []);
}

/** The inline header telltale: a tone-coloured dot + the plain-language word,
 * the forensic detail one hover/focus away in a Tooltip (same disclosure the
 * adjacent ProfileChip uses). Colour is reinforcement; the word stands alone
 * (PRODUCT.md status grammar). Carries its own provider so it's safe anywhere. */
export function DurabilityReadout({ summary }: { summary: DurabilitySummary }) {
  const color =
    summary.tone === "nominal"
      ? "var(--color-instrument-nominal)"
      : "var(--color-instrument-caution)";
  return (
    <TooltipProvider delayDuration={100}>
      <Tooltip>
        <TooltipTrigger asChild>
          <span className="inline-flex items-center gap-1.5 text-muted-foreground">
            <span
              aria-hidden
              className="size-1.5 shrink-0 rounded-full"
              style={{ backgroundColor: color }}
            />
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
  return (
    <Text variant="label" tone="muted" className="mb-2.5 block text-[0.65rem]">
      {children}
    </Text>
  );
}

export function DiagnosticsDrawer({
  session,
  sessionId,
  eventCount,
}: {
  session: Session;
  sessionId: string;
  eventCount: number;
}) {
  return (
    <Sheet>
      <SheetTrigger asChild>
        <Button variant="outline" size="sm">
          <Activity />
          Diagnostics
        </Button>
      </SheetTrigger>
      {/* Full-width on phones (the old rail was desktop-only), a fixed panel on
          sm+. gap-0 so the sections own their own rhythm. */}
      <SheetContent side="right" className="w-full gap-0 sm:max-w-md">
        <SheetHeader className="shrink-0 border-b px-5">
          <SheetTitle className="font-display text-base font-semibold tracking-tight">
            Diagnostics
          </SheetTitle>
          <SheetDescription>
            Durability, recovery checkpoints, and live operations for this session.
          </SheetDescription>
        </SheetHeader>

        <div className="min-h-0 flex-1 space-y-6 overflow-y-auto px-5 py-5">
          {/* "Show the receipts": the durability detail, in-context beside the
              session it describes. */}
          <section>
            <SectionLabel>durability</SectionLabel>
            <SessionCowState sessionId={sessionId} />
            <div className="mt-3">
              <DurabilityTimeline sessionId={sessionId} />
            </div>
          </section>

          <section className="border-t pt-4">
            <SectionLabel>session</SectionLabel>
            <dl className="space-y-2.5 text-sm">
              <div>
                <dt className="text-muted-foreground">image</dt>
                <dd className="mt-0.5 font-mono text-[0.8rem] break-all text-foreground">
                  {session.image}
                </dd>
              </div>
              <MetricRow label="created" value={`${relativeTime(session.created_at)} ago`} />
              {/* The count keeps its own element: any poller reads
                  Number(textContent) of exactly this span. */}
              <MetricRow
                label="events"
                value={<span data-testid="event-count">{eventCount}</span>}
              />
            </dl>
          </section>

          {/* Admin live-ops — self-gating (admin + Active only), so a non-admin
              sees a clean read-only ledger above and nothing here. */}
          <TeleportControl session={session} />
          <PauseResumeControl session={session} />
        </div>
      </SheetContent>
    </Sheet>
  );
}

// ---------------------------------------------------------------------------
// Admin live-ops (ADR 0045 Phase F) — relocated from the session-detail rail.
// ---------------------------------------------------------------------------

// Teleport (live-migrate) an Active session onto a chosen host. Today the verb
// rides the snapshot-rehome evac pipeline (a brief pause); ADR 0045 Phase C
// swaps it to post-copy live migration under the same control. Renders nothing
// unless the viewer is an admin and the session is Active (the only relocatable
// state).
function TeleportControl({ session }: { session: Session }) {
  const isAdmin = useIsAdmin();
  const { data: hosts } = useHosts();
  const teleport = useTeleportSession(session.id);
  const [target, setTarget] = useState<string>("");

  if (!isAdmin || session.status !== "active") return null;

  const candidates = (hosts ?? []).filter((h) => h.status === "ready" && h.id !== session.host_id);

  return (
    <div className="border-t pt-4">
      <SectionLabel>teleport</SectionLabel>
      {candidates.length === 0 ? (
        <Text tone="muted" className="text-xs">
          no other ready host available
        </Text>
      ) : (
        <div className="space-y-2">
          <Select value={target} onValueChange={setTarget}>
            <SelectTrigger className="h-8 text-xs">
              <SelectValue placeholder="destination host…" />
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
            disabled={!target || teleport.isPending}
            onClick={() => teleport.mutate(target)}
          >
            {teleport.isPending ? "teleporting…" : "Teleport"}
          </Button>
        </div>
      )}
    </div>
  );
}

// Freeze / unfreeze this session's microVM in place — the admin affordance to
// drive + observe the pause/flush path. Does not change session state (the row
// stays `active`), so both buttons are always offered; the operator picks.
// Admin-only, Active-only.
function PauseResumeControl({ session }: { session: Session }) {
  const isAdmin = useIsAdmin();
  const { pause, resume } = usePauseResumeSession(session.id);

  if (!isAdmin || session.status !== "active") return null;

  return (
    <div className="border-t pt-4">
      <SectionLabel>freeze</SectionLabel>
      <div className="flex gap-2">
        <Button
          size="sm"
          variant="secondary"
          className="flex-1"
          disabled={pause.isPending}
          onClick={() => pause.mutate()}
        >
          {pause.isPending ? "pausing…" : "Pause"}
        </Button>
        <Button
          size="sm"
          variant="secondary"
          className="flex-1"
          disabled={resume.isPending}
          onClick={() => resume.mutate()}
        >
          {resume.isPending ? "resuming…" : "Resume"}
        </Button>
      </div>
    </div>
  );
}
