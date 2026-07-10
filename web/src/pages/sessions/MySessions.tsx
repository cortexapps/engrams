import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { Plus } from "lucide-react";
import { useDebouncedValue } from "../../hooks/useDebouncedValue";
import { useTasksAsSessionList } from "../../hooks/useTasks";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { PageHeading } from "../../components/page-heading";
import { SessionsList } from "./sessions-list";

const PAGE_SIZE = 50;

// The full "My tasks" list — the searchable history that backs the
// start screen's "See all". Starting a task now lives on the composer at
// /sessions; this page's "New task" is just a link back to it.
export function MySessions() {
  const [search, setSearch] = useState("");
  const [page, setPage] = useState(1);
  const debouncedSearch = useDebouncedValue(search);
  const {
    data: sessions,
    totalCount,
    isPending,
    error,
  } = useTasksAsSessionList({
    scope: "mine",
    search: debouncedSearch,
    page,
    pageSize: PAGE_SIZE,
  });
  const total = totalCount ?? 0;
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
          setPage(1);
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

      {/* Also shown past page 1 with a shrunken total (live poll can drop
          matches out from under us) so Prev is always reachable. */}
      {(total > PAGE_SIZE || page > 1) && (
        <div className="flex items-center justify-between gap-4">
          <p className="font-mono text-xs tabular-nums text-muted-foreground">
            {(page - 1) * PAGE_SIZE + 1}–{Math.min(page * PAGE_SIZE, total)} of {total}
          </p>
          <div className="flex gap-2">
            <Button
              variant="outline"
              onClick={() => setPage((value) => value - 1)}
              disabled={page === 1}
            >
              Prev
            </Button>
            <Button
              variant="outline"
              onClick={() => setPage((value) => value + 1)}
              disabled={page * PAGE_SIZE >= total}
            >
              Next
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
