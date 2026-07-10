import { useState } from "react";
import { Check, RotateCcw, X } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import { errorMessage } from "../../lib/errors";
import { useUpdateTask } from "../../hooks/useTasks";

/**
 * Inline title editor for a task. Save sets the STICKY custom title; Reset
 * (shown only when the current title is already custom) clears it back to the
 * auto title (the harness suggestion, then the truncated prompt). Enter saves,
 * Escape cancels. The mutation invalidates the task list so every surface
 * re-renders with the new title.
 */
export function TitleEditForm({
  taskId,
  initial,
  isCustom,
  onDone,
  className,
  inputClassName,
}: {
  taskId: string;
  initial: string;
  isCustom: boolean;
  onDone: () => void;
  className?: string;
  inputClassName?: string;
}) {
  const [value, setValue] = useState(initial);
  const update = useUpdateTask();

  const save = () => {
    const trimmed = value.trim();
    // Empty is a no-op (use Reset to clear a custom title) — the server rejects
    // a blank title, so short-circuit here instead of surfacing an error.
    if (trimmed === "") {
      onDone();
      return;
    }
    update.mutate(
      { taskId, title: trimmed },
      { onSuccess: () => onDone(), onError: (e) => toast.error(errorMessage(e)) },
    );
  };

  const reset = () => {
    // Title omitted → reset to auto.
    update.mutate(
      { taskId },
      { onSuccess: () => onDone(), onError: (e) => toast.error(errorMessage(e)) },
    );
  };

  return (
    <div className={cn("flex items-center gap-1", className)}>
      <Input
        autoFocus
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            save();
          } else if (e.key === "Escape") {
            e.preventDefault();
            onDone();
          }
        }}
        disabled={update.isPending}
        aria-label="Session title"
        className={cn("h-7 py-1", inputClassName)}
      />
      <Button
        type="button"
        size="icon-xs"
        variant="ghost"
        onClick={save}
        disabled={update.isPending}
        aria-label="Save title"
      >
        <Check />
      </Button>
      {isCustom && (
        <Button
          type="button"
          size="icon-xs"
          variant="ghost"
          onClick={reset}
          disabled={update.isPending}
          title="Reset to the auto-generated title"
          aria-label="Reset to auto title"
        >
          <RotateCcw />
        </Button>
      )}
      <Button
        type="button"
        size="icon-xs"
        variant="ghost"
        onClick={onDone}
        disabled={update.isPending}
        aria-label="Cancel"
      >
        <X />
      </Button>
    </div>
  );
}
