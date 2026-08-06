import { useRef, useState } from "react";
import { Activity, UserRound, X } from "lucide-react";
import { StatusGlyph } from "../../components/Glyph";
import { useDebouncedValue } from "../../hooks/useDebouncedValue";
import { useAdminUsersMap, useTasksInfiniteAsSessionList } from "../../hooks/useTasks";
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
  "unreachable",
  "parked",
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
  const scrollRef = useRef<HTMLDivElement>(null);
  const [search, setSearch] = useState("");
  const [filters, setFilters] = useState<Record<string, string[]>>({ owner: [], state: [] });
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
    hasNextPage,
    isFetchingNextPage,
    fetchNextPage,
    isPending,
    error,
  } = useTasksInfiniteAsSessionList(
    {
      scope: "all",
      search: debouncedSearch,
      createdByUserIds: filters.owner,
      states: filters.state,
    },
    PAGE_SIZE,
  );
  const total = totalCount ?? 0;
  const visibleCount = sessions?.length ?? 0;
  const hasFilters = Boolean(search || filters.owner.length || filters.state.length);
  const loadMoreRef = useLoadMoreSentinel({
    hasMore: hasNextPage,
    isFetching: isFetchingNextPage,
    onLoadMore: fetchNextPage,
  });

  const clearFilters = () => {
    setSearch("");
    setFilters({ owner: [], state: [] });
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-6 p-4 md:p-6">
      <PageHeading title="All tasks" />

      <div className="flex flex-wrap items-center gap-2">
        <Input
          value={search}
          onChange={(event) => setSearch(event.target.value)}
          placeholder="Search tasks…"
          aria-label="Search tasks"
          className="max-w-xs"
        />
        <FilterBar fields={fields} value={filters} onChange={setFilters} />
        {hasFilters && (
          <Button variant="ghost" onClick={clearFilters}>
            <X />
            Clear
          </Button>
        )}
      </div>

      <div ref={scrollRef} className="min-h-0 flex-1 space-y-6 overflow-auto">
        <SessionsList
          sessions={sessions ?? []}
          isPending={isPending}
          error={error}
          showOwner
          emptyText={hasFilters ? "No matching tasks." : "No tasks across the fleet yet."}
          scrollRef={scrollRef}
        />
        {hasNextPage && (
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
    </div>
  );
}
