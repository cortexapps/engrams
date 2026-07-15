import { useMemo, useState } from "react";
import { timestampDate } from "@bufbuild/protobuf/wkt";
import { Link, useNavigate } from "@tanstack/react-router";
import { ArchiveIcon, ArchiveRestoreIcon, HammerIcon, WrenchIcon } from "lucide-react";
import { toast } from "sonner";

import { PageHeading } from "../../components/page-heading";
import { ProfileChip } from "../../components/profiles/ProfileChip";
import {
  useArchivePapercut,
  usePapercuts,
  useStartFixTask,
  useUnarchivePapercut,
} from "../../hooks/usePapercuts";
import { useTasks } from "../../hooks/useTasks";
import { useNow } from "../../hooks/useNow";
import { errorMessage } from "../../lib/errors";
import type { Papercut } from "../../gen/engram/app/v1/papercut_pb";
import { relativeTime } from "../sessions/session-format";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { cn } from "@/lib/utils";

function CreatedAt({ papercut, now }: { papercut: Papercut; now: number }) {
  if (!papercut.createdAt) return null;
  const createdAt = timestampDate(papercut.createdAt);

  return (
    <span
      className="font-mono text-xs tabular-nums text-muted-foreground"
      title={createdAt.toLocaleString()}
    >
      {relativeTime(createdAt.toISOString(), now)} ago
    </span>
  );
}

function PapercutRow({
  papercut,
  fixSessionId,
  now,
  busy,
  onStartFix,
  onArchive,
  onUnarchive,
}: {
  papercut: Papercut;
  fixSessionId?: string;
  now: number;
  busy: boolean;
  onStartFix: (id: string) => Promise<void>;
  onArchive: (id: string) => Promise<void>;
  onUnarchive: (id: string) => Promise<void>;
}) {
  const [descriptionExpanded, setDescriptionExpanded] = useState(false);

  return (
    <article
      className={cn("rounded-lg border bg-card p-4 shadow-xs", papercut.archived && "opacity-75")}
      data-testid={`papercut-${papercut.id}`}
    >
      <div className="flex flex-col gap-4 lg:flex-row lg:items-start">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-base font-medium">{papercut.summary}</h2>
            <Badge variant="outline">{papercut.category || "uncategorized"}</Badge>
            {papercut.severity && <Badge variant="secondary">{papercut.severity}</Badge>}
            {papercut.archived && <Badge variant="secondary">archived</Badge>}
          </div>

          <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-2 text-xs text-muted-foreground">
            <ProfileChip profile={papercut.profile} className="text-xs" />
            <CreatedAt papercut={papercut} now={now} />
            {papercut.sessionId && (
              <Link
                to="/sessions/$id"
                params={{ id: papercut.sessionId }}
                className="underline decoration-border underline-offset-4 transition-colors hover:text-foreground"
              >
                from session
              </Link>
            )}
            {papercut.fixTaskId &&
              (fixSessionId ? (
                <Badge asChild variant="secondary">
                  <Link
                    to="/sessions/$id"
                    params={{ id: fixSessionId }}
                    title={`Fix task ${papercut.fixTaskId}`}
                  >
                    <WrenchIcon />
                    fix task
                  </Link>
                </Badge>
              ) : (
                <Badge variant="secondary" title={`Fix task ${papercut.fixTaskId}`}>
                  <WrenchIcon />
                  fix task
                </Badge>
              ))}
          </div>

          {papercut.tags.length > 0 && (
            <div className="mt-3 flex flex-wrap gap-1.5">
              {papercut.tags.map((tag, index) => (
                <span
                  key={`${tag}-${index}`}
                  className="rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground"
                >
                  {tag}
                </span>
              ))}
            </div>
          )}

          {papercut.description && (
            <button
              type="button"
              aria-expanded={descriptionExpanded}
              className={cn(
                "mt-3 block w-full cursor-pointer text-left text-sm leading-relaxed text-muted-foreground transition-colors hover:text-foreground",
                !descriptionExpanded && "line-clamp-3",
              )}
              onClick={() => setDescriptionExpanded((expanded) => !expanded)}
            >
              {papercut.description}
            </button>
          )}
        </div>

        <div className="flex shrink-0 flex-wrap items-center gap-1 lg:justify-end">
          {!papercut.archived && (
            <Button size="sm" disabled={busy} onClick={() => void onStartFix(papercut.id)}>
              <HammerIcon />
              Start fix task
            </Button>
          )}

          {papercut.archived ? (
            <Button
              variant="ghost"
              size="sm"
              disabled={busy}
              onClick={() => void onUnarchive(papercut.id)}
            >
              <ArchiveRestoreIcon />
              Unarchive
            </Button>
          ) : (
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button variant="ghost" size="sm" disabled={busy}>
                  <ArchiveIcon />
                  Archive
                </Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Archive “{papercut.summary}”?</AlertDialogTitle>
                  <AlertDialogDescription>
                    It will be hidden from the default papercuts view. You can show and unarchive it
                    later.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction onClick={() => void onArchive(papercut.id)}>
                    Archive papercut
                  </AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          )}
        </div>
      </div>
    </article>
  );
}

export function Papercuts() {
  const [showArchived, setShowArchived] = useState(false);
  const { data, error, isPending } = usePapercuts(showArchived);
  const { data: taskData } = useTasks({ scope: "mine" });
  const archivePapercut = useArchivePapercut();
  const unarchivePapercut = useUnarchivePapercut();
  const startFixTask = useStartFixTask();
  const navigate = useNavigate();
  const now = useNow();

  const fixSessions = useMemo(
    () =>
      new Map(
        (taskData?.tasks ?? []).flatMap((task) => {
          const sessionId = task.sessions[0]?.sessionId;
          return sessionId ? [[task.id, sessionId] as const] : [];
        }),
      ),
    [taskData],
  );

  const onStartFix = async (id: string) => {
    try {
      const response = await startFixTask.mutateAsync({ id });
      if (!response.sessionId) throw new Error("Fix task started, but no session was returned.");
      toast.success("Fix task started");
      navigate({ to: "/sessions/$id", params: { id: response.sessionId } });
    } catch (err) {
      toast.error(errorMessage(err));
    }
  };

  const onArchive = async (id: string) => {
    try {
      await archivePapercut.mutateAsync({ id });
      toast.success("Papercut archived");
    } catch (err) {
      toast.error(errorMessage(err));
    }
  };

  const onUnarchive = async (id: string) => {
    try {
      await unarchivePapercut.mutateAsync({ id });
      toast.success("Papercut unarchived");
    } catch (err) {
      toast.error(errorMessage(err));
    }
  };

  const papercuts = data?.papercuts ?? [];
  const busy = archivePapercut.isPending || unarchivePapercut.isPending || startFixTask.isPending;

  return (
    <div className="flex flex-col gap-6">
      <PageHeading
        title="Papercuts"
        eyebrow="Kaizen · Agent feedback"
        description="Small frictions agents encounter while working, collected so they can be fixed deliberately."
        actions={
          <label htmlFor="show-archived-papercuts" className="flex items-center gap-2 text-sm">
            <Switch
              id="show-archived-papercuts"
              checked={showArchived}
              onCheckedChange={setShowArchived}
            />
            Show archived
          </label>
        }
      />

      {isPending && <p className="text-sm text-muted-foreground">Loading…</p>}

      {!isPending && error && papercuts.length === 0 && (
        <div role="alert" className="rounded-lg border border-dashed p-8 text-center">
          <p className="text-sm text-destructive">Couldn’t load papercuts. {errorMessage(error)}</p>
        </div>
      )}

      {!isPending && !error && papercuts.length === 0 && (
        <div className="rounded-lg border border-dashed p-8 text-center">
          <p className="text-sm text-muted-foreground">
            Agents log papercuts — small frictions they hit while working. None yet.
          </p>
        </div>
      )}

      <div className="flex flex-col gap-3">
        {papercuts.map((papercut) => (
          <PapercutRow
            key={papercut.id}
            papercut={papercut}
            fixSessionId={fixSessions.get(papercut.fixTaskId)}
            now={now}
            busy={busy}
            onStartFix={onStartFix}
            onArchive={onArchive}
            onUnarchive={onUnarchive}
          />
        ))}
      </div>
    </div>
  );
}
