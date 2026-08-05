import { useMemo } from "react";
import {
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
import { ExposedPortsSection } from "./ports/ExposedPortsSection";
import { ProfileChip } from "./profiles/ProfileChip";
import { extractFileChanges } from "./session-thread/fileChanges";

export interface OverviewPaneProps {
  sessionId: string;
  taskId: string | null;
  session: Session | undefined;
  events: IndexedEvent[];
  profile: ProfileSnapshotView | null;
  onShowChanges: () => void;
}

export function OverviewPane({
  sessionId,
  taskId,
  session,
  events,
  profile,
  onShowChanges,
}: OverviewPaneProps) {
  const files = useMemo(() => extractFileChanges(events), [events]);
  const totals = useMemo(
    () =>
      files.reduce(
        (sum, file) => ({
          additions: sum.additions + file.additions,
          deletions: sum.deletions + file.deletions,
        }),
        { additions: 0, deletions: 0 },
      ),
    [files],
  );
  const { data } = usePrRefs(taskId, sessionId);
  const prRefs = data?.prRefs ?? [];

  if (!session) return null;

  return (
    <div className="h-full min-h-0 overflow-auto">
      <div className="divide-y divide-border/60">
        <div className="flex items-start gap-3 px-4 py-3">
          <PackageIcon className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
          <div className="min-w-0 flex-1 space-y-2">
            {profile && <ProfileChip profile={profile} />}
            <div className="flex min-w-0 flex-wrap items-center gap-2">
              <span className="min-w-0 truncate font-mono text-xs text-muted-foreground">
                {session.image}
              </span>
              <Badge variant="secondary">{session.mode}</Badge>
            </div>
            {profile && profile.skills.length > 0 && (
              <div className="flex flex-wrap gap-1.5">
                {profile.skills.map((skill) => (
                  <Badge key={skill} variant="outline" className="px-1.5 py-0 text-[11px]">
                    {skill}
                  </Badge>
                ))}
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
