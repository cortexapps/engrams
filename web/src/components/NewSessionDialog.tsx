import { type ComponentProps, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useMutation, createConnectQueryKey } from "@connectrpc/connect-query";
import { useForm } from "react-hook-form";
import { Check, KeyRound } from "lucide-react";
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
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Field, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Textarea } from "@/components/ui/textarea";

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

  const { data, isPending, error, refetch } = useProfiles(false);
  const profiles = data?.profiles ?? [];
  const qc = useQueryClient();
  const createTaskMutation = useMutation(createTask);

  const [selectedId, setSelectedId] = useState<string | null>(null);
  const form = useForm<{ prompt: string }>({ defaultValues: { prompt: "" } });

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
            New task
          </Button>
        </DialogTrigger>
      )}
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New task</DialogTitle>
          <DialogDescription>Pick a profile, then say what to run.</DialogDescription>
        </DialogHeader>

        {isPending && <p className="text-sm text-muted-foreground">Loading profiles…</p>}

        {!isPending && error && (
          <div className="flex flex-col items-start gap-2 py-2">
            <p className="text-sm text-destructive">Couldn’t load profiles.</p>
            <Button type="button" variant="outline" size="sm" onClick={() => refetch?.()}>
              Retry
            </Button>
          </div>
        )}

        {!isPending && !error && profiles.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No profiles configured — contact an admin to set one up.
          </p>
        )}

        {!isPending && !error && profiles.length > 0 && (
          <form onSubmit={form.handleSubmit(onSubmit)}>
            <FieldGroup>
              {/* cmdk drives the picker: one tab stop, type-to-filter, ↑/↓ to
                  move the cursor, Enter to choose the highlighted profile. The
                  list is a real listbox/option tree (not a hand-rolled
                  radiogroup), so the keyboard + screen-reader contract is the
                  one cmdk ships across the app (⌘K, the icon picker). */}
              <Command loop label="Profiles" className="rounded-md border">
                <CommandInput placeholder="Search profiles…" autoFocus />
                <CommandList className="max-h-64">
                  <CommandEmpty>No matches.</CommandEmpty>
                  <CommandGroup>
                    {profiles.map((p) => {
                      const isChosen = selectedId === p.id;
                      return (
                        <CommandItem
                          key={p.id}
                          value={`${p.name} ${p.description}`}
                          data-testid={`profile-row-${p.id}`}
                          onSelect={() => setSelectedId(p.id)}
                          className="items-start gap-3 py-2.5"
                        >
                          <ProfileIcon
                            name={p.icon}
                            className="mt-0.5 size-5 shrink-0 text-muted-foreground"
                          />
                          <span className="flex min-w-0 flex-1 flex-col gap-0.5">
                            <span className="font-medium text-foreground">
                              {p.name}
                              {isChosen && <span className="sr-only"> (selected)</span>}
                            </span>
                            <span className="truncate text-sm text-muted-foreground">
                              {p.description}
                            </span>
                          </span>
                          {isChosen && (
                            <Check aria-hidden className="mt-0.5 size-4 shrink-0 text-foreground" />
                          )}
                        </CommandItem>
                      );
                    })}
                  </CommandGroup>
                </CommandList>
              </Command>

              <Field>
                <FieldLabel htmlFor="prompt">Task</FieldLabel>
                <Textarea
                  id="prompt"
                  rows={2}
                  placeholder="Describe the task…"
                  {...form.register("prompt")}
                />
              </Field>

              {/* The credential risk, reinforced where it's assumed: the
                  developer launching the session, not just the admin editing
                  the profile. */}
              {selected?.includeUserTokens && (
                <p className="flex items-center gap-1.5 text-sm text-foreground">
                  <KeyRound className="size-3.5 shrink-0 text-instrument-caution" />
                  This profile carries your Claude token into the sandbox.
                </p>
              )}

              {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
            </FieldGroup>

            <DialogFooter className="mt-4">
              <Button
                type="submit"
                data-testid="start-session"
                disabled={!selected || form.formState.isSubmitting}
              >
                {form.formState.isSubmitting ? "Starting…" : "Start task"}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  );
}
