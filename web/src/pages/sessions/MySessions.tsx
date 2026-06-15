import { useNavigate } from "@tanstack/react-router";
import { useTasksAsSessionList } from "../../hooks/useTasks";
import { useAuth } from "../../auth/AuthProvider";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { NewSessionDialog } from "../../components/NewSessionDialog";
import { PageHeading } from "../../components/page-heading";
import { SessionsList } from "./sessions-list";

export function MySessions() {
  const { principal } = useAuth();
  const { data: sessions, isPending, error } = useTasksAsSessionList();
  const navigate = useNavigate();
  const showTokenNudge = !principal.is_admin && !principal.has_claude_token;

  return (
    <div className="flex-1 space-y-6 overflow-auto p-4 md:p-6">
      <PageHeading
        title="Sessions"
        description="Bounded units of agent work: launch, watch, resume."
        actions={
          <NewSessionDialog
            triggerTestId="new-session"
            onCreated={(id) => navigate({ to: "/sessions/$id", params: { id } })}
          />
        }
      />

      {showTokenNudge && (
        <Card>
          <CardContent className="flex items-center justify-between gap-4 py-3">
            <span className="text-sm">
              No Claude Code token saved yet — built-in Claude sessions need one.
            </span>
            <Button
              variant="secondary"
              size="sm"
              onClick={() => navigate({ to: "/settings/tokens" })}
            >
              Add token
            </Button>
          </CardContent>
        </Card>
      )}

      <SessionsList
        sessions={sessions ?? []}
        isPending={isPending}
        error={error}
        showOwner={false}
        emptyText="No sessions yet. Start one to launch a sandbox and hand an agent a task."
        emptyAction={
          <NewSessionDialog onCreated={(id) => navigate({ to: "/sessions/$id", params: { id } })} />
        }
      />
    </div>
  );
}
