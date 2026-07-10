import { useState } from "react";
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
  const [page, setPage] = useState(1);
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
    page,
    pageSize: PAGE_SIZE,
  });
  const total = totalCount ?? 0;
  const hasFilters = Boolean(search || filters.owner.length || filters.state.length);

  const clearFilters = () => {
    setSearch("");
    setFilters({ owner: [], state: [] });
    setPage(1);
  };

  return (
    <div className="flex-1 space-y-6 overflow-auto p-4 md:p-6">
      <PageHeading title="All tasks" description="Every task across the fleet, owner-attributed." />

      <div className="flex flex-wrap items-center gap-2">
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
        <FilterBar
          fields={fields}
          value={filters}
          onChange={(next) => {
            setFilters(next);
            setPage(1);
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
