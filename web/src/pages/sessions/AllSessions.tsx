import { useCallback, useState } from "react";
import { Activity, UserRound, X } from "lucide-react";
import { StatusGlyph } from "../../components/Glyph";
import { useDebouncedValue } from "../../hooks/useDebouncedValue";
import { useAdminUsersMap, useTasksAsSessionListWithOwners } from "../../hooks/useTasks";
import type { SessionState } from "../../lib/types";
import { PageHeading } from "../../components/page-heading";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FilterBar, type FilterField } from "./filter-bar";
import { SessionsList } from "./sessions-list";
import { statusLabel } from "./session-format";
import { useLoadMoreSentinel } from "../../hooks/useLoadMoreSentinel";

const PAGE_SIZE = 50;
const SESSION_STATES: SessionState[] = [
  "active",
  "idle",
  "pending",
  "queued",
  "created",
  "evacuating",
  "evicting",
  "host_lost",
  "completed",
  "failed",
  "dead",
];

export function AllSessions() {
  const [search, setSearch] = useState("");
  const [filters, setFilters] = useState<Record<string, string[]>>({ owner: [], state: [] });
  const [limit, setLimit] = useState(PAGE_SIZE);
  const debouncedSearch = useDebouncedValue(search);
  const { data: usersMap } = useAdminUsersMap(true);
  const fields: FilterField[] = [
    {
      key: "owner",
      label: "Owner",
      icon: <UserRound className="size-4 text-muted-foreground" />,
      options: [
        ...Array.from(usersMap ?? []).map(([value, user]) => ({
          value,
          label: user.name || user.email,
        })),
        { value: "system", label: "System / unattributed" },
      ],
    },
    {
      key: "state",
      label: "State",
      icon: <Activity className="size-4 text-muted-foreground" />,
      options: SESSION_STATES.map((state) => ({
        value: state,
        label: statusLabel(state),
        icon: <StatusGlyph status={state} />,
      })),
    },
  ];
  const {
    data: sessions,
    totalCount,
    isPending,
    error,
  } = useTasksAsSessionListWithOwners({
    scope: "all",
    search: debouncedSearch,
    createdByUserIds: filters.owner,
    states: filters.state,
    page: 1,
    pageSize: limit,
  });
  const total = totalCount ?? 0;
  const visibleCount = sessions?.length ?? 0;
  const hasMore = visibleCount < total;
  const hasFilters = Boolean(search || filters.owner.length || filters.state.length);
  // Row count is intentionally a dependency: it re-observes a still-visible
  // sentinel after the larger response has rendered.
  const handleLoadMore = useCallback(() => setLimit((value) => value + PAGE_SIZE), [visibleCount]);
  const loadMoreRef = useLoadMoreSentinel({ hasMore, onLoadMore: handleLoadMore });

  const clearFilters = () => {
    setSearch("");
    setFilters({ owner: [], state: [] });
    setLimit(PAGE_SIZE);
  };

  return (
    <div className="flex-1 space-y-6 overflow-auto p-4 md:p-6">
      <PageHeading title="All tasks" description="Every task across the fleet, owner-attributed." />

      <div className="flex flex-wrap items-center gap-2">
        <Input
          value={search}
          onChange={(event) => {
            setSearch(event.target.value);
            setLimit(PAGE_SIZE);
          }}
          placeholder="Search tasks…"
          aria-label="Search tasks"
          className="max-w-xs"
        />
        <FilterBar
          fields={fields}
          value={filters}
          onChange={(next) => {
            setFilters(next);
            setLimit(PAGE_SIZE);
          }}
        />
        {hasFilters && (
          <Button variant="ghost" onClick={clearFilters}>
            <X />
            Clear
          </Button>
        )}
      </div>

      <SessionsList
        sessions={sessions ?? []}
        isPending={isPending}
        error={error}
        showOwner
        emptyText={hasFilters ? "No matching tasks." : "No tasks across the fleet yet."}
      />
      {hasMore && (
        <div ref={loadMoreRef} className="py-2 text-center text-xs text-muted-foreground">
          Loading more…
        </div>
      )}
      {total > 0 && (
        <p className="font-mono text-xs tabular-nums text-muted-foreground">
          {visibleCount} of {total} tasks
        </p>
      )}
    </div>
  );
}
