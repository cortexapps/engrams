import { type ComponentProps, useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import { createSession } from "../api";
import { useAuth } from "../auth/AuthProvider";
import { useEnabledImages } from "../hooks/useEnabledImages";
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
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Textarea } from "@/components/ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

const newSessionSchema = z.object({
  image: z.string().min(1, "Select an image"),
  mode: z.enum(["agent", "dev_vm"]),
  prompt: z.string(),
});
type NewSessionValues = z.infer<typeof newSessionSchema>;

export function NewSessionDialog({
  onCreated,
  variant,
  className,
  open: openProp,
  onOpenChange,
  showTrigger = true,
}: {
  onCreated: (id: string) => void;
  /** Trigger styling. Defaults to the primary (lime) button; the sessions rail
   * passes `secondary` + `w-full` so it reads quietly beside the active row. */
  variant?: ComponentProps<typeof Button>["variant"];
  className?: string;
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
  const { data: images, isLoading } = useEnabledImages(true);
  const { principal } = useAuth();
  const qc = useQueryClient();
  const navigate = useNavigate();

  const form = useForm<NewSessionValues>({
    resolver: zodResolver(newSessionSchema),
    defaultValues: { image: "", mode: "agent", prompt: "" },
  });
  const selectedUri = form.watch("image");
  const mode = form.watch("mode");

  const selected = images?.find((i) => i.image_uri === selectedUri);
  useEffect(() => {
    if (!selectedUri && images && images.length > 0) {
      form.setValue("image", images[0].image_uri);
    }
  }, [images, selectedUri, form]);

  const harnessName = selected?.harness_name ?? null;
  const hasHarness = harnessName !== null;
  const isClaude = harnessName === "claude";
  const promptMeaningful = hasHarness && mode === "agent";
  const needsToken = isClaude && mode === "agent" && !principal.has_claude_token;
  const canSubmit = !!selected && !form.formState.isSubmitting && !needsToken;

  const onSubmit = async (data: NewSessionValues) => {
    if (!selected) return;
    try {
      const res = await createSession({
        image: selected.image_uri,
        mode: data.mode === "dev_vm" ? "dev_vm" : undefined,
        prompt: promptMeaningful ? data.prompt.trim() || undefined : undefined,
      });
      qc.invalidateQueries({ queryKey: ["sessions"] });
      setOpen(false);
      form.reset();
      onCreated(res.session_id);
    } catch (e) {
      form.setError("root", { message: e instanceof Error ? e.message : String(e) });
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      {showTrigger && (
        <DialogTrigger asChild>
          <Button data-testid="new-session" variant={variant} className={className}>
            New session
          </Button>
        </DialogTrigger>
      )}
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New session</DialogTitle>
          <DialogDescription>Launch a bounded unit of agent work.</DialogDescription>
        </DialogHeader>

        {isLoading && <p className="text-sm text-muted-foreground">Loading images…</p>}
        {images && images.length === 0 && (
          <p className="text-sm text-muted-foreground">
            {principal.role === "admin" ? (
              <>
                No images enabled yet. Enable one in{" "}
                <Link
                  to="/operator/images"
                  className="underline underline-offset-4 hover:text-foreground"
                >
                  Operator → Images
                </Link>{" "}
                before launching a session.
              </>
            ) : (
              "No images enabled yet. Ask an admin to enable one before you can launch a session."
            )}
          </p>
        )}

        {images && images.length > 0 && (
          <form onSubmit={form.handleSubmit(onSubmit)}>
            <FieldGroup>
              <Controller
                name="image"
                control={form.control}
                render={({ field }) => (
                  <Field>
                    <FieldLabel htmlFor={field.name}>Image</FieldLabel>
                    <Select value={field.value} onValueChange={field.onChange}>
                      <SelectTrigger id={field.name} data-testid="image-select">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        {images.map((i) => (
                          <SelectItem key={i.image_uri} value={i.image_uri}>
                            {i.image_uri}
                            {i.manifest_name ? ` — ${i.manifest_name}` : ""}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                    <FieldDescription>
                      {harnessName
                        ? `Baked harness: ${harnessName}`
                        : "No baked harness — shell-only image"}
                    </FieldDescription>
                  </Field>
                )}
              />

              <Controller
                name="mode"
                control={form.control}
                render={({ field }) => (
                  <Field>
                    <FieldLabel htmlFor={field.name}>Mode</FieldLabel>
                    <Select value={field.value} onValueChange={field.onChange}>
                      <SelectTrigger id={field.name}>
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="agent">agent — drive the baked harness</SelectItem>
                        <SelectItem value="dev_vm">dev VM — shell-only</SelectItem>
                      </SelectContent>
                    </Select>
                  </Field>
                )}
              />

              {promptMeaningful && (
                <Controller
                  name="prompt"
                  control={form.control}
                  render={({ field }) => (
                    <Field>
                      <FieldLabel htmlFor={field.name}>Prompt</FieldLabel>
                      <Textarea
                        {...field}
                        id={field.name}
                        rows={2}
                        placeholder="optional opening prompt"
                      />
                    </Field>
                  )}
                />
              )}

              {needsToken && (
                <FieldDescription>
                  This image runs built-in Claude, which uses your saved token — you don’t have one
                  yet.
                </FieldDescription>
              )}
              {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
            </FieldGroup>

            <DialogFooter className="mt-4">
              {needsToken ? (
                <Button
                  type="button"
                  variant="secondary"
                  onClick={() => {
                    setOpen(false);
                    navigate({ to: "/settings/tokens" });
                  }}
                >
                  Save your Claude token
                </Button>
              ) : (
                <Button type="submit" data-testid="start-session" disabled={!canSubmit}>
                  {form.formState.isSubmitting ? "Starting…" : "Start"}
                </Button>
              )}
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  );
}
