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
import { statusLabel } from "../pages/sessions/session-format";
import { StatusGlyph } from "./Glyph";
import { ExposedPortsSection } from "./ports/ExposedPortsSection";
import { ProfileChip } from "./profiles/ProfileChip";
import { extractFileChanges, totalCounts } from "./session-thread/fileChanges";

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
  onShowChanges: () => void;
}

export function OverviewPane({
  sessionId,
  taskId,
  session,
  events,
  profile,
  selection,
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
    <div className="h-full min-h-0 overflow-auto">
      <div className="divide-y divide-border/60">
        <div className="flex items-start gap-3 px-4 py-3">
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
            className="flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-accent/50"
          >
            <FileDiffIcon className="size-4 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 text-sm">
              {files.length} {files.length === 1 ? "file" : "files"} changed
            </span>
            <span className="flex shrink-0 items-center gap-2 font-mono text-xs tabular-nums">
              <span className="text-emerald-600 dark:text-emerald-400">+{totals.additions}</span>
              <span className="text-red-600 dark:text-red-400">−{totals.deletions}</span>
            </span>
          </button>
        )}

        <div className="px-4 py-3">
          {prRefs.length === 0 ? (
            <div className="flex items-center gap-3">
              <GitPullRequestArrowIcon className="size-4 shrink-0 text-muted-foreground" />
              <p className="text-sm text-muted-foreground italic">No pull requests yet.</p>
            </div>
          ) : (
            <div className="space-y-3">
              {prRefs.map((prRef) => (
                <div key={prRef.id} className="flex items-start gap-3">
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
            </div>
          )}
        </div>

        <div className="flex items-start gap-3 px-4 py-3">
          <RadioTowerIcon className="mt-1.5 size-4 shrink-0 text-muted-foreground" />
          <div className="min-w-0 flex-1">
            <ExposedPortsSection sessionId={sessionId} active={session.status === "active"} />
          </div>
        </div>
      </div>
    </div>
  );
}
