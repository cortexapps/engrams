import { Fragment } from "react";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { useSessionCheckpoints } from "../hooks/useCheckpoints";
import { fmtAgo, fmtBytes } from "../format";
import type { CheckpointSummary } from "../types";

// ADR 0028 A.log / Fix A: the consolidated durability view for a
// session — the recovery ladder made legible.
//
//   - A horizontal sparkline: each retained checkpoint is a dot on a
//     hairline. The newest is the ringed nominal dot — the always-pinned
//     rung-1 recovery anchor ("if the host died now, you'd warm-resume
//     here"). Older points are quiet; any not-yet-verified point reads
//     caution-amber.
//   - A chain list below: every retained checkpoint (the forkable
//     history window), newest first, with age + size + a status badge.
//
// Status colour comes from the instrument vocabulary (nominal / caution),
// never the lime accent, and is always paired with the badge text so the
// state never rides on colour alone.
//
// Deliberately NOT in the transcript: periodic checkpoints land ~1/min;
// flooding the conversation with them would drown it. The transcript keeps
// the semantic moments (eviction snapshot, resume, the recovery boundary);
// the steady cadence lives here.

type Tone = "anchor" | "recoverable" | "unverified";

function toneOf(c: CheckpointSummary): Tone {
  if (c.is_latest) return "anchor";
  return c.recoverable ? "recoverable" : "unverified";
}

const DOT: Record<Tone, string> = {
  anchor: "bg-instrument-nominal",
  recoverable: "bg-muted-foreground/40",
  unverified: "border border-instrument-caution bg-card",
};

const BADGE: Record<Tone, string> = {
  anchor: "border-instrument-nominal/40 text-instrument-nominal",
  recoverable: "text-muted-foreground",
  unverified: "border-instrument-caution/40 text-instrument-caution",
};

const LABEL: Record<Tone, string> = {
  anchor: "anchor",
  recoverable: "recoverable",
  unverified: "unverified",
};

export function DurabilityTimeline({ sessionId }: { sessionId: string }) {
  const { data, isLoading } = useSessionCheckpoints(sessionId);
  const checkpoints = data?.checkpoints ?? [];

  if (isLoading && checkpoints.length === 0) {
    return null;
  }
  if (checkpoints.length === 0) {
    return (
      <p className="text-xs text-muted-foreground italic">
        no checkpoints yet — recovery falls back to the latest disk flush.
      </p>
    );
  }

  // Render oldest→newest left-to-right (the API hands newest-first).
  const ordered = [...checkpoints].reverse();

  return (
    <div className="space-y-2">
      <TimelineStrip ordered={ordered} />
      <ChainList checkpoints={checkpoints} />
    </div>
  );
}

function TimelineStrip({ ordered }: { ordered: CheckpointSummary[] }) {
  return (
    <div
      className="flex items-center gap-1.5 overflow-x-auto py-1"
      title="recovery ladder (oldest → newest); the ringed dot is the rung-1 anchor"
    >
      {ordered.map((c, i) => {
        const tone = toneOf(c);
        return (
          <Fragment key={c.snapshot_id}>
            {i > 0 && <span aria-hidden className="h-px w-2 shrink-0 bg-border" />}
            <span
              aria-hidden
              title={`${fmtAgo(c.created_at)} · ${fmtBytes(c.size_bytes)} · ${LABEL[tone]}`}
              className={cn(
                "size-2 shrink-0 rounded-full",
                DOT[tone],
                c.is_latest && "ring-2 ring-instrument-nominal/25",
              )}
            />
          </Fragment>
        );
      })}
    </div>
  );
}

function ChainList({ checkpoints }: { checkpoints: CheckpointSummary[] }) {
  return (
    <ul className="text-xs">
      {checkpoints.map((c) => {
        const tone = toneOf(c);
        return (
          <li
            key={c.snapshot_id}
            className="flex items-center gap-2 border-b border-dashed border-border py-1.5 last:border-0"
            title={`snapshot ${c.snapshot_id} · events cursor ${c.events_cursor ?? "unresolved"}`}
          >
            <span aria-hidden className={cn("size-1.5 shrink-0 rounded-full", DOT[tone])} />
            <span className="font-mono tabular-nums text-foreground">{fmtAgo(c.created_at)}</span>
            <span className="ml-auto font-mono tabular-nums text-muted-foreground">
              {fmtBytes(c.size_bytes)}
            </span>
            <Badge
              variant="outline"
              className={cn("shrink-0 px-1.5 py-0 text-[0.65rem] font-normal", BADGE[tone])}
            >
              {LABEL[tone]}
            </Badge>
          </li>
        );
      })}
    </ul>
  );
}
