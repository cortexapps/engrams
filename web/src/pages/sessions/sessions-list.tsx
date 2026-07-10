import { useState, type ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import { Pencil } from "lucide-react";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { StatusGlyph } from "../../components/Glyph";
import { TitleEditForm } from "./TitleEditForm";
import type { SessionListItem } from "../../lib/types";
import {
  compareSessions,
  lifecycleOf,
  matchesFilter,
  relativeTime,
  shortId,
  statusLabel,
  type StatusFilter,
} from "./session-format";
import { ProfileChip } from "../../components/profiles/ProfileChip";

// The sessions list reads as a workspace switcher, not a data grid: a flat list
// of rich rows ordered most-recently-active first (the same order as the rail),
// each row a single focusable link into that workspace. A status telltale (the glyph)
// leads, the session id carries identity — the slot a human-readable name will
// take over later — and the image/status/age trail as quiet metadata. Live /
// Archived / All tabs keep terminal history out of the default working set.

const FILTERS: StatusFilter[] = ["live", "archived", "all"];

export function SessionsList({
  sessions,
  showOwner,
  emptyText,
  emptyAction,
  isPending,
  error,
}: {
  sessions: SessionListItem[];
  showOwner: boolean;
  /** Shown when the account has no sessions at all (not when a filter is empty). */
  emptyText: string;
  /** Primary action co-located with the first-run empty state (e.g. New session). */
  emptyAction?: ReactNode;
  /** First-load (no data yet) → skeleton rows instead of a false-empty flash. */
  isPending?: boolean;
  /** Fetch error. Only surfaced when there's no data to fall back to; a failed
   * background refetch keeps the last-good list visible (calm under live state). */
  error?: unknown;
}) {
  const [filter, setFilter] = useState<StatusFilter>("live");

  // The query keeps `placeholderData: prev`, so once we've loaded, `isPending`
  // is false and stale rows stay on screen through refetches. These two guards
  // therefore only fire on the genuine first load.
  if (isPending) return <SkeletonRows />;
  if (error && sessions.length === 0) {
    return (
      <div role="alert" className="rounded-lg border border-dashed py-12 text-center">
        <p className="text-sm text-destructive">
          Couldn’t load tasks.{error instanceof Error ? ` ${error.message}` : ""}
        </p>
      </div>
    );
  }

  if (sessions.length === 0) {
    return (
      <div className="flex flex-col items-center gap-3 rounded-lg border border-dashed py-16 text-center">
        <span className="text-xl leading-none text-muted-foreground">
          <StatusGlyph status="idle" beat={false} />
        </span>
        <p className="max-w-sm text-sm text-muted-foreground">{emptyText}</p>
        {emptyAction}
      </div>
    );
  }

  const live = sessions.filter((s) => lifecycleOf(s.status) !== "ARCHIVED").length;

  return (
    <Tabs value={filter} onValueChange={(v) => setFilter(v as StatusFilter)}>
      <TabsList>
        <TabsTrigger value="live">
          Live
          <Count n={live} />
        </TabsTrigger>
        <TabsTrigger value="archived">
          Archived
          <Count n={sessions.length - live} />
        </TabsTrigger>
        <TabsTrigger value="all">
          All
          <Count n={sessions.length} />
        </TabsTrigger>
      </TabsList>
      {FILTERS.map((f) => (
        <TabsContent key={f} value={f}>
          <SessionRows sessions={sessions} filter={f} showOwner={showOwner} />
        </TabsContent>
      ))}
    </Tabs>
  );
}

function SessionRows({
  sessions,
  filter,
  showOwner,
}: {
  sessions: SessionListItem[];
  filter: StatusFilter;
  showOwner: boolean;
}) {
  const rows = sessions.filter((s) => matchesFilter(s.status, filter)).sort(compareSessions);
  if (rows.length === 0) {
    return (
      <p className="py-10 text-center text-sm text-muted-foreground">
        {filter === "archived" ? "No archived tasks." : "No live tasks."}
      </p>
    );
  }
  return (
    <ul className="divide-y divide-border overflow-hidden rounded-lg border">
      {rows.map((s) => (
        <SessionRow key={s.id} s={s} showOwner={showOwner} />
      ))}
    </ul>
  );
}

function Count({ n }: { n: number }) {
  return <span className="ml-1.5 font-mono text-xs tabular-nums opacity-60">{n}</span>;
}

// First-load placeholder: the row silhouette, not a spinner, so the list keeps
// its shape while the fetch resolves (mirrors the rail's skeleton behaviour).
function SkeletonRows() {
  return (
    <div role="status" aria-label="Loading tasks">
      <ul className="divide-y divide-border overflow-hidden rounded-lg border">
        {Array.from({ length: 5 }).map((_, i) => (
          <li key={i} className="flex items-center gap-3 px-3 py-2.5">
            <Skeleton className="size-2.5 shrink-0 rounded-full" />
            <Skeleton className="h-4 w-24 shrink-0" />
            <Skeleton className="h-3 w-32" />
            <span className="flex-1" />
            <Skeleton className="h-3 w-14 shrink-0" />
            <Skeleton className="h-3 w-7 shrink-0" />
          </li>
        ))}
      </ul>
    </div>
  );
}

/** Owner column label. Service-account owners (ADR 0086 API keys, email
 *  `apikey+…@service.local`) display their user NAME — which IS the key's
 *  name (e.g. `ci-engineering-blog`) — instead of the synthetic email. */
function ownerLabel(s: SessionListItem): string | null {
  const email = s.owner_email ?? "";
  if (email.startsWith("apikey+") && email.endsWith("@service.local")) {
    return s.owner_name || email;
  }
  return s.owner_email;
}

function SessionRow({ s, showOwner }: { s: SessionListItem; showOwner: boolean }) {
  const [editing, setEditing] = useState(false);
  // A real task row (not a synthetic unattributed-* admin row) can be renamed.
  // The server is authoritative (owner or admin); the list only surfaces rows
  // the caller may see, so showing the control whenever there's a task is safe.
  const canEdit = s.taskId != null;

  if (editing && s.taskId) {
    return (
      <li data-testid="session-row" data-session-id={s.id} className="px-3 py-2">
        <TitleEditForm
          taskId={s.taskId}
          initial={s.title ?? ""}
          isCustom={s.titleIsCustom}
          onDone={() => setEditing(false)}
        />
      </li>
    );
  }

  return (
    <li data-testid="session-row" data-session-id={s.id} className="group relative">
      <Link
        to="/sessions/$id"
        params={{ id: s.id }}
        title={s.id}
        className="flex items-center gap-3 px-3 py-2.5 outline-none transition-colors hover:bg-accent/60 focus-visible:bg-accent/60 focus-visible:ring-2 focus-visible:ring-ring/50 focus-visible:ring-inset"
      >
        <span className="inline-flex w-3 shrink-0 justify-center text-[0.7rem] leading-none">
          <StatusGlyph status={s.status} />
        </span>
        {/* Identity: the title carries the row (falling back to the short id
            when unnamed); the image trails as the recessive "what kind" descriptor. */}
        <span className="flex min-w-0 flex-1 items-baseline gap-2.5">
          {s.title ? (
            <span className="min-w-0 truncate text-sm font-medium">{s.title}</span>
          ) : (
            <span className="shrink-0 font-mono text-sm font-medium">{shortId(s.id)}</span>
          )}
          <ProfileChip profile={s.profile} fallbackImage={s.image} className="text-xs" />
        </span>
        {showOwner && (
          <span className="hidden w-40 shrink-0 items-center gap-2 sm:flex">
            {s.owner_kind === "system" ? (
              <span className="text-xs italic text-muted-foreground">system</span>
            ) : (
              <>
                <Avatar className="size-5 shrink-0">
                  <AvatarFallback className="text-[10px]">
                    {(s.owner_name || s.owner_email || "?").charAt(0).toUpperCase()}
                  </AvatarFallback>
                </Avatar>
                <span className="min-w-0 truncate font-mono text-xs text-muted-foreground">
                  {ownerLabel(s)}
                </span>
              </>
            )}
          </span>
        )}
        <span className="w-20 shrink-0 text-xs text-muted-foreground">{statusLabel(s.status)}</span>
        <span className="w-9 shrink-0 text-right font-mono text-xs tabular-nums text-muted-foreground">
          {relativeTime(s.last_active_at)}
        </span>
      </Link>
      {/* Rename affordance — a sibling of the Link (never nested in an anchor),
          revealed on row hover / keyboard focus. */}
      {canEdit && (
        <Button
          type="button"
          size="icon-xs"
          variant="ghost"
          onClick={() => setEditing(true)}
          aria-label="Rename session"
          title="Rename"
          className="absolute right-2 top-1/2 -translate-y-1/2 opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100"
        >
          <Pencil />
        </Button>
      )}
    </li>
  );
}
