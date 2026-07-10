import type { ComponentPropsWithoutRef, ReactNode, Ref, RefObject } from "react";
import { Link } from "@tanstack/react-router";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Skeleton } from "@/components/ui/skeleton";
import { StatusGlyph } from "../../components/Glyph";
import type { SessionListItem } from "../../lib/types";
import { relativeTime, shortId, statusLabel } from "./session-format";
import { ProfileChip } from "../../components/profiles/ProfileChip";
import { useNow } from "../../hooks/useNow";

// The sessions list reads as a workspace switcher, not a data grid: a flat list
// of rich rows in the server's most-recently-active-first order,
// each row a single focusable link into that workspace. A status telltale (the glyph)
// leads, the title carries identity (short id when unnamed), and the
// image/status/age trail as quiet metadata.

export function SessionsList({
  sessions,
  showOwner,
  emptyText,
  emptyAction,
  isPending,
  error,
  scrollRef,
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
  /** Scroll container shared with the virtualized session rows. */
  scrollRef: RefObject<HTMLElement | null>;
}) {
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

  return <SessionRows sessions={sessions} showOwner={showOwner} scrollRef={scrollRef} />;
}

export function SessionRows({
  sessions,
  showOwner,
  scrollRef,
}: {
  sessions: SessionListItem[];
  showOwner: boolean;
  scrollRef: RefObject<HTMLElement | null>;
}) {
  const now = useNow();
  const virtualizer = useVirtualizer({
    count: sessions.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 41,
    overscan: 12,
    initialRect: { width: 800, height: 600 },
  });

  return (
    <ul
      className="relative overflow-hidden rounded-lg border"
      style={{ height: virtualizer.getTotalSize() }}
    >
      {virtualizer.getVirtualItems().map((virtualRow) => {
        const s = sessions[virtualRow.index];
        return (
          <SessionRow
            key={s.id}
            ref={virtualizer.measureElement}
            data-index={virtualRow.index}
            className={
              virtualRow.index === sessions.length - 1 ? undefined : "border-b border-border"
            }
            style={{ transform: `translateY(${virtualRow.start}px)` }}
            s={s}
            showOwner={showOwner}
            now={now}
          />
        );
      })}
    </ul>
  );
}

// First-load placeholder: the row silhouette, not a spinner, so the list keeps
// its shape while the fetch resolves (mirrors the rail's skeleton behaviour).
export function SkeletonRows() {
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

/** Owner column label: display name first, email as the fallback. The same
 *  rule covers service-account owners (ADR 0086 API keys) — their user NAME is
 *  the key's name (e.g. `ci-engineering-blog`) while the email is the
 *  synthetic `apikey+…@service.local`. */
function ownerLabel(s: SessionListItem): string | null {
  return s.owner_name || s.owner_email;
}

// Rows are read-only: renaming lives on the session detail page, not the list
// (a hover-revealed control fought the virtualized rows' transforms and full-
// row Link — retired rather than patched).
export function SessionRow({
  s,
  showOwner,
  now,
  ref,
  ...rowProps
}: {
  s: SessionListItem;
  showOwner: boolean;
  now?: number;
} & ComponentPropsWithoutRef<"li"> & { ref?: Ref<HTMLLIElement> }) {
  return (
    <li
      ref={ref}
      {...rowProps}
      data-testid="session-row"
      data-session-id={s.id}
      className={`absolute left-0 top-0 w-full ${rowProps.className ?? ""}`}
    >
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
                {/* Names read as language, not machine data — no mono (the
                    email fallback inherits the same quiet tone). */}
                <span className="min-w-0 truncate text-xs text-muted-foreground">
                  {ownerLabel(s)}
                </span>
              </>
            )}
          </span>
        )}
        <span className="w-20 shrink-0 text-xs text-muted-foreground">{statusLabel(s.status)}</span>
        <span className="w-9 shrink-0 text-right font-mono text-xs tabular-nums text-muted-foreground">
          {relativeTime(s.last_active_at, now)}
        </span>
      </Link>
    </li>
  );
}
