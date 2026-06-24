import { Link } from "@tanstack/react-router";
import { Plus } from "lucide-react";
import { useTasksAsSessionList } from "../../hooks/useTasks";
import { Button } from "@/components/ui/button";
import { PageHeading } from "../../components/page-heading";
import { SessionsList } from "./sessions-list";

// The full "My tasks" table — the dense, filterable history that backs the
// start screen's "See all". Starting a task now lives on the composer at
// /sessions; this page's "New task" is just a link back to it.
export function MySessions() {
  const { data: sessions, isPending, error } = useTasksAsSessionList();
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

      <SessionsList
        sessions={sessions ?? []}
        isPending={isPending}
        error={error}
        showOwner={false}
        emptyText="No tasks yet. Start one to launch a sandbox and hand an agent a task."
        emptyAction={newTask}
      />
    </div>
  );
}
