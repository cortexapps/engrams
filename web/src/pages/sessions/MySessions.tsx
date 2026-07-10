import { useCallback, useState } from "react";
import { Link } from "@tanstack/react-router";
import { Plus } from "lucide-react";
import { useDebouncedValue } from "../../hooks/useDebouncedValue";
import { useTasksAsSessionList } from "../../hooks/useTasks";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { PageHeading } from "../../components/page-heading";
import { SessionsList } from "./sessions-list";
import { useLoadMoreSentinel } from "../../hooks/useLoadMoreSentinel";

const PAGE_SIZE = 50;

// The full "My tasks" list — the searchable history that backs the
// start screen's "See all". Starting a task now lives on the composer at
// /sessions; this page's "New task" is just a link back to it.
export function MySessions() {
  const [search, setSearch] = useState("");
  const [limit, setLimit] = useState(PAGE_SIZE);
  const debouncedSearch = useDebouncedValue(search);
  const {
    data: sessions,
    totalCount,
    isPending,
    error,
  } = useTasksAsSessionList({
    scope: "mine",
    search: debouncedSearch,
    page: 1,
    pageSize: limit,
  });
  const total = totalCount ?? 0;
  const visibleCount = sessions?.length ?? 0;
  const hasMore = visibleCount < total;
  // Row count is intentionally a dependency: it re-observes a still-visible
  // sentinel after the larger response has rendered.
  const handleLoadMore = useCallback(() => setLimit((value) => value + PAGE_SIZE), [visibleCount]);
  const loadMoreRef = useLoadMoreSentinel({ hasMore, onLoadMore: handleLoadMore });
  const newTask = (
    <Button asChild>
      <Link to="/sessions">
        <Plus className="size-4" />
        New task
      </Link>
    </Button>
  );

  return (
    <div className="flex-1 space-y-6 overflow-auto p-4 md:p-6">
      <PageHeading
        title="My tasks"
        description="Bounded units of agent work: launch, watch, resume."
        actions={newTask}
      />

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

      <SessionsList
        sessions={sessions ?? []}
        isPending={isPending}
        error={error}
        showOwner={false}
        emptyText={
          search
            ? "No matching tasks."
            : "No tasks yet. Start one to launch a sandbox and hand an agent a task."
        }
        emptyAction={search ? undefined : newTask}
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
