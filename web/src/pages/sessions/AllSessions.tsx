import { useTasksAsSessionList } from "../../hooks/useTasks";
import { PageHeading } from "../../components/page-heading";
import { SessionsList } from "./sessions-list";

export function AllSessions() {
  const { data: sessions, isPending, error } = useTasksAsSessionList();
  return (
    <div className="flex-1 space-y-6 overflow-auto p-4 md:p-6">
      <PageHeading
        title="All sessions"
        description="Every session across the fleet, owner-attributed."
      />
      <SessionsList
        sessions={sessions ?? []}
        isPending={isPending}
        error={error}
        showOwner
        emptyText="No sessions across the fleet yet."
      />
    </div>
  );
}
