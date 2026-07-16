import { useState } from "react";
import { timestampDate } from "@bufbuild/protobuf/wkt";
import { Link } from "@tanstack/react-router";
import { ArchiveIcon, ArchiveRestoreIcon, CopyIcon } from "lucide-react";
import { toast } from "sonner";

import { PageHeading } from "../../components/page-heading";
import { ProfileChip } from "../../components/profiles/ProfileChip";
import { useArchivePapercut, usePapercuts, useUnarchivePapercut } from "../../hooks/usePapercuts";
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

function papercutDetails(papercut: Papercut): string {
  if (!papercut.createdAt) throw new Error("Papercut is missing its logged date.");

  const lines = [`Papercut: ${papercut.summary}`, `Category: ${papercut.category}`];
  const severity = papercut.severity.trim();
  if (severity) lines.push(`Severity: ${severity}`);
  const tags = papercut.tags.map((tag) => tag.trim()).filter(Boolean);
  if (tags.length > 0) lines.push(`Tags: ${tags.join(", ")}`);
  lines.push(
    `Logged: ${timestampDate(papercut.createdAt).toISOString()} from session ${papercut.sessionId}`,
  );
  return `${lines.join("\n")}\n\n${papercut.description}`;
}

function PapercutRow({
  papercut,
  now,
  busy,
  onArchive,
  onUnarchive,
}: {
  papercut: Papercut;
  now: number;
  busy: boolean;
  onArchive: (id: string) => Promise<void>;
  onUnarchive: (id: string) => Promise<void>;
}) {
  const [descriptionExpanded, setDescriptionExpanded] = useState(false);

  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(papercutDetails(papercut));
      toast.success("Papercut copied");
    } catch (err) {
      toast.error(errorMessage(err));
    }
  };

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
          <Button variant="ghost" size="sm" onClick={() => void onCopy()}>
            <CopyIcon />
            Copy papercut details
          </Button>

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
  const archivePapercut = useArchivePapercut();
  const unarchivePapercut = useUnarchivePapercut();
  const now = useNow();

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
  const busy = archivePapercut.isPending || unarchivePapercut.isPending;

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
            now={now}
            busy={busy}
            onArchive={onArchive}
            onUnarchive={onUnarchive}
          />
        ))}
      </div>
    </div>
  );
}
