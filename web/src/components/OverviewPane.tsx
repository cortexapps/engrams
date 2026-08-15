import { useMemo } from "react";
import {
  BotIcon,
  ExternalLinkIcon,
  FileDiffIcon,
  GitPullRequestArrowIcon,
  PackageIcon,
  RadioTowerIcon,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { usePrRefs } from "../hooks/usePrRefs";
import type { IndexedEvent } from "../events";
import type { ProfileSnapshotView, Session } from "../lib/types";
import type { Task } from "../gen/engram/app/v1/task_pb";
import { statusLabel } from "../pages/sessions/session-format";
import { StatusGlyph } from "./Glyph";
import { SessionAppsSection } from "./apps/SessionAppsSection";
import { ProfileChip } from "./profiles/ProfileChip";
import { extractFileChanges, totalCounts } from "./session-thread/fileChanges";
import { TaskTreeNavigation } from "./TaskTreeNavigation";

export interface OverviewSelection {
  harness?: string;
  model?: string;
  effort?: string;
}

export interface OverviewPaneProps {
  sessionId: string;
  taskId: string | null;
  session: Session | undefined;
  events: IndexedEvent[];
  profile: ProfileSnapshotView | null;
  selection: OverviewSelection | null;
  taskTree?: Task | null;
  onShowChanges: () => void;
}

export function OverviewPane({
  sessionId,
  taskId,
  session,
  events,
  profile,
  selection,
  taskTree,
  onShowChanges,
}: OverviewPaneProps) {
  const files = useMemo(() => extractFileChanges(events), [events]);
  const totals = totalCounts(files);
  const { data } = usePrRefs(taskId, sessionId);
  const prRefs = data?.prRefs ?? [];
  const selectionLabel = selection
    ? [selection.harness, selection.model, selection.effort].filter(Boolean).join(" · ")
    : "";

  if (!session) return null;

  return (
    // Cards, not `divide-y` bands. A band bounded by one hairline reads as
    // chrome — you cannot see where it starts or ends, so the pull request and
    // the diff summary looked like parts of the same undifferentiated strip.
    // Each of these is a distinct fact about the run, so each gets edges,
    // a radius, and its own padding.
    <div className="h-full min-h-0 overflow-auto">
      <div className="flex flex-col gap-3 p-3">
        <TaskTreeNavigation rootTask={taskTree} currentSessionId={sessionId} />

        <div className="flex items-start gap-3 rounded-lg border bg-card p-3">
          <PackageIcon className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
          <div className="min-w-0 flex-1 space-y-2">
            {/* The dense inline chip: the image URI stays behind its hover
                disclosure. The mode badge appears only off the default —
                "agent" on every row is noise. */}
            {profile ? (
              <div className="flex min-w-0 items-center gap-2 text-sm">
                <ProfileChip profile={profile} className="min-w-0" />
                {session.mode !== "agent" && <Badge variant="secondary">{session.mode}</Badge>}
              </div>
            ) : (
              <div className="flex min-w-0 flex-wrap items-center gap-2">
                <span className="min-w-0 truncate font-mono text-xs text-muted-foreground">
                  {session.image}
                </span>
                {session.mode !== "agent" && <Badge variant="secondary">{session.mode}</Badge>}
              </div>
            )}
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <StatusGlyph status={session.status} />
              <span>{statusLabel(session.status)}</span>
            </div>
            {selectionLabel && (
              <div className="flex min-w-0 items-center gap-2 font-mono text-xs text-muted-foreground">
                <BotIcon className="size-3.5 shrink-0" />
                <span className="truncate">{selectionLabel}</span>
              </div>
            )}
          </div>
        </div>

        {files.length > 0 && (
          <button
            type="button"
            onClick={onShowChanges}
            className="flex w-full items-center gap-3 rounded-lg border bg-card p-3 text-left transition-colors hover:bg-accent"
          >
            <FileDiffIcon className="size-4 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 text-sm">
              {files.length} {files.length === 1 ? "file" : "files"} changed
            </span>
            {/* The status INK tokens, not raw palette greens and reds. The work
                pane re-tones these for the green ground; hard-coded Tailwind
                `emerald-600` measures 2.30:1 there and `red-600` 1.77:1. */}
            <span className="flex shrink-0 items-center gap-2 font-mono text-xs tabular-nums">
              <span className="text-instrument-nominal-ink">+{totals.additions}</span>
              <span className="text-instrument-critical-ink">−{totals.deletions}</span>
            </span>
          </button>
        )}

        {prRefs.length === 0 ? (
          <div className="flex items-center gap-3 rounded-lg border border-dashed p-3">
            <GitPullRequestArrowIcon className="size-4 shrink-0 text-muted-foreground" />
            <p className="text-sm text-muted-foreground">No pull requests yet.</p>
          </div>
        ) : (
          // One card per pull request, not one card holding a list of them —
          // each PR is its own thing to open.
          <>
            {prRefs.map((prRef) => (
              <div key={prRef.id} className="flex items-start gap-3 rounded-lg border bg-card p-3">
                <GitPullRequestArrowIcon className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
                <div className="min-w-0 flex-1">
                  <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-1">
                    <span className="shrink-0 font-mono text-xs text-muted-foreground">
                      {prRef.repo}#{prRef.prNumber}
                    </span>
                    {prRef.url ? (
                      <a
                        href={prRef.url}
                        target="_blank"
                        rel="noreferrer"
                        className="group inline-flex min-w-0 items-center gap-1 text-sm font-medium hover:underline"
                      >
                        <span className="truncate">{prRef.title || "Pull request"}</span>
                        <ExternalLinkIcon className="size-3.5 shrink-0 text-muted-foreground" />
                      </a>
                    ) : (
                      <span className="min-w-0 truncate text-sm font-medium">
                        {prRef.title || "Pull request"}
                      </span>
                    )}
                  </div>
                  {prRef.headBranch && prRef.baseBranch && (
                    <div className="mt-1 flex min-w-0 items-center gap-1.5 font-mono text-xs text-muted-foreground">
                      <span className="truncate">{prRef.headBranch}</span>
                      <span aria-hidden>→</span>
                      <span className="truncate">{prRef.baseBranch}</span>
                    </div>
                  )}
                </div>
              </div>
            ))}
          </>
        )}

        <div className="flex items-start gap-3 rounded-lg border bg-card p-3">
          <RadioTowerIcon className="mt-1.5 size-4 shrink-0 text-muted-foreground" />
          <div className="min-w-0 flex-1">
            <SessionAppsSection sessionId={sessionId} active={session.status === "active"} />
          </div>
        </div>
      </div>
    </div>
  );
}
