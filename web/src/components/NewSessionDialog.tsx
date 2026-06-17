import { type ComponentProps, useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useMutation, createConnectQueryKey } from "@connectrpc/connect-query";
import { useForm } from "react-hook-form";
import { createTask, listTasks } from "../gen/engram/app/v1/task-TaskService_connectquery";
import { useProfiles } from "../hooks/useProfiles";
import { ProfileIcon } from "./profiles/ProfileIcon";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Field, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";

export function NewSessionDialog({
  onCreated,
  variant,
  className,
  triggerTestId,
  open: openProp,
  onOpenChange,
  showTrigger = true,
}: {
  onCreated: (id: string) => void;
  variant?: ComponentProps<typeof Button>["variant"];
  className?: string;
  /** Test id for the trigger button. Pass it from at most ONE mounted instance
   * per page (the header actions today) — a second instance with the same id
   * breaks strict-mode getByTestId when both render (e.g. empty list + header). */
  triggerTestId?: string;
  /** Controlled open state. Omit for the self-contained trigger usage; pass it
   * (with `showTrigger={false}`) for the global, keyboard/palette-driven mount
   * in RootLayout. */
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  showTrigger?: boolean;
}) {
  const [internalOpen, setInternalOpen] = useState(false);
  const open = openProp ?? internalOpen;
  const setOpen = onOpenChange ?? setInternalOpen;

  const { data, isPending } = useProfiles(false);
  const profiles = data?.profiles ?? [];
  const qc = useQueryClient();
  const createTaskMutation = useMutation(createTask);

  const [search, setSearch] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const form = useForm<{ prompt: string }>({ defaultValues: { prompt: "" } });

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return profiles;
    return profiles.filter(
      (p) => p.name.toLowerCase().includes(q) || p.description.toLowerCase().includes(q),
    );
  }, [profiles, search]);

  const selected = profiles.find((p) => p.id === selectedId) ?? null;

  const onSubmit = async (values: { prompt: string }) => {
    if (!selected) return;
    try {
      const res = await createTaskMutation.mutateAsync({
        type: "chat",
        profileId: selected.id,
        prompt: values.prompt.trim() ? values.prompt.trim() : undefined,
      });
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listTasks, cardinality: "finite" }),
      });
      const sessionId = res.task?.sessions[0]?.sessionId;
      if (sessionId) {
        setOpen(false);
        form.reset();
        setSelectedId(null);
        setSearch("");
        onCreated(sessionId);
      } else {
        form.setError("root", { message: "Task created but no session id returned." });
      }
    } catch (e) {
      form.setError("root", { message: e instanceof Error ? e.message : String(e) });
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      {showTrigger && (
        <DialogTrigger asChild>
          <Button data-testid={triggerTestId} variant={variant} className={className}>
            New session
          </Button>
        </DialogTrigger>
      )}
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New session</DialogTitle>
          <DialogDescription>Pick a profile, then say what to run.</DialogDescription>
        </DialogHeader>

        {isPending && <p className="text-sm text-muted-foreground">Loading profiles…</p>}

        {!isPending && profiles.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No profiles configured — contact an admin to set one up.
          </p>
        )}

        {!isPending && profiles.length > 0 && (
          <form onSubmit={form.handleSubmit(onSubmit)}>
            <FieldGroup>
              <Input
                placeholder="Search profiles…"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                autoFocus
              />

              <div
                className="flex max-h-72 flex-col gap-2 overflow-y-auto"
                role="radiogroup"
                aria-label="Profiles"
              >
                {filtered.length === 0 && (
                  <p className="px-1 py-4 text-center text-sm text-muted-foreground">No matches.</p>
                )}
                {filtered.map((p) => (
                  <button
                    type="button"
                    key={p.id}
                    role="radio"
                    aria-checked={selectedId === p.id}
                    data-testid={`profile-row-${p.id}`}
                    onClick={() => setSelectedId(p.id)}
                    className={cn(
                      "flex items-start gap-3 rounded-md border p-3 text-left transition-colors",
                      selectedId === p.id
                        ? "border-primary bg-accent"
                        : "border-border hover:bg-accent/50",
                    )}
                  >
                    <ProfileIcon
                      name={p.icon}
                      className="mt-0.5 size-5 shrink-0 text-muted-foreground"
                    />
                    <span className="min-w-0">
                      <span className="block font-medium">{p.name}</span>
                      <span className="block truncate text-sm text-muted-foreground">
                        {p.description}
                      </span>
                      {p.includeUserTokens && (
                        <span className="mt-1 block text-xs text-muted-foreground">
                          carries your token
                        </span>
                      )}
                    </span>
                  </button>
                ))}
              </div>

              <Field>
                <FieldLabel htmlFor="prompt">Task</FieldLabel>
                <Textarea
                  id="prompt"
                  rows={2}
                  placeholder="Describe the task for this session…"
                  {...form.register("prompt")}
                />
              </Field>

              {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
            </FieldGroup>

            <DialogFooter className="mt-4">
              <Button
                type="submit"
                data-testid="start-session"
                disabled={!selected || form.formState.isSubmitting}
              >
                {form.formState.isSubmitting ? "Starting…" : "Start session"}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  );
}
