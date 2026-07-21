import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { Trash2 } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { errorMessage } from "../../lib/errors";
import { useDeleteTask } from "../../hooks/useTasks";

/**
 * Delete-session control for the SessionDetail masthead. Deletes the owning
 * TASK (DeleteTask), which tears down every session it owns via DeleteSession
 * and removes the task row, so the work drops out of all list surfaces — the
 * declutter the button promises.
 *
 * Guarded by a confirmation dialog because delete is terminal: the sandbox is
 * destroyed and the session can't be resumed. On success we leave the (now
 * gone) detail page for the list; errors surface as a toast and keep the dialog
 * open. Only rendered when a real `taskId` exists — synthetic `unattributed-*`
 * admin rows have no task and DeleteTask would 404.
 */
export function DeleteSessionButton({ taskId, title }: { taskId: string; title: string | null }) {
  const navigate = useNavigate();
  const del = useDeleteTask();
  const [open, setOpen] = useState(false);
  const label = title?.trim() ? `"${title.trim()}"` : "this session";

  const confirm = () => {
    del.mutate(
      { taskId },
      {
        onSuccess: () => {
          setOpen(false);
          toast.success("Session deleted");
          navigate({ to: "/sessions/list" });
        },
        onError: (e) => toast.error(errorMessage(e)),
      },
    );
  };

  return (
    <AlertDialog open={open} onOpenChange={(next) => !del.isPending && setOpen(next)}>
      <AlertDialogTrigger asChild>
        <Button variant="outline" size="sm">
          <Trash2 />
          Delete
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Delete {label}?</AlertDialogTitle>
          <AlertDialogDescription>
            This tears down the sandbox and permanently removes the session. This can't be undone.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={del.isPending}>Cancel</AlertDialogCancel>
          <AlertDialogAction
            // Keep the dialog mounted through the request (Radix closes on click
            // by default); we drive open-state ourselves so the pending button
            // stays visible and errors keep the dialog up.
            onClick={(e) => {
              e.preventDefault();
              confirm();
            }}
            disabled={del.isPending}
            className="bg-destructive text-white hover:bg-destructive/90"
          >
            {del.isPending ? "Deleting…" : "Delete session"}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
