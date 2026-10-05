import { useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import { AlertTriangle } from "lucide-react";

import { Link } from "@tanstack/react-router";

import {
  useBuiltinAutomation,
  useSetInputValue,
  useSetMapInputEntry,
} from "../../hooks/useAutomations";
import { useProfiles } from "../../hooks/useProfiles";
import { toast } from "sonner";
import { errorMessage } from "../../lib/errors";
import { PageHeading } from "../../components/page-heading";
import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { Badge } from "@/components/ui/badge";
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
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
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
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";

// The repositories engrams reviews (ADR 0100), as a page of the Reviews
// product. The list IS the PR-review automation's `repos` input (ADR 0119
// phase 4.7b: the single source — no enrollment table beside it): enrolling
// a repo writes one entry of that map and enables the automation, the way
// the Slack page writes a channel override — the product is the front of
// the platform, and the link below opens the back. `mode` decides whether a
// PR is reviewed automatically on open (auto) or only when the app is
// @mentioned / dispatched (on_request); `autofix` whether posted findings
// route back for fixes. The review sessions run on the reviewer profile
// picked below: the automation's `profile` input, written one key at a time
// so a stale page never overwrites another admin's edit.
export function ReviewRepositories() {
  const { data: profileData } = useProfiles();
  const builtin = useBuiltinAutomation("pr_review");
  const setProfile = useSetInputValue();
  const profiles = profileData?.profiles ?? [];
  const automation = builtin.data?.automation;
  const automationId = automation?.id;
  const rows = reposOf(automation?.inputsJson);
  const isPending = builtin.isPending;
  const error = builtin.error;
  const reviewerProfile = profileOf(automation?.inputsJson);

  const onReviewerProfile = async (profileId: string) => {
    if (!automationId) return;
    try {
      await setProfile.mutateAsync({
        automationId,
        inputKey: "profile",
        valueJson: JSON.stringify(profileId === NO_PROFILE ? "" : profileId),
      });
      toast.success("Reviewer profile saved");
    } catch (e) {
      toast.error(errorMessage(e));
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-6 overflow-auto p-4 md:p-6">
      <PageHeading
        title="Repositories"
        count={rows.length > 0 ? `${rows.length} enrolled` : undefined}
        actions={automationId ? <EnrollDialog automationId={automationId} /> : undefined}
      />

      <p className="text-sm text-muted-foreground">
        Pull requests on these repositories are reviewed by the{" "}
        {automationId ? (
          <Link
            to="/automations/$id"
            params={{ id: automationId }}
            search={{ tab: "activity" }}
            className="underline underline-offset-2"
          >
            PR review automation
          </Link>
        ) : (
          "PR review automation"
        )}
        . Every pull request it touches is a workstream; every pass is a run.
      </p>

      <section className="flex flex-col gap-3" aria-label="Reviewer profile">
        <h2 className="text-base font-semibold">Reviewer profile</h2>
        <p className="text-sm text-muted-foreground">
          The session profile the finder and verifier workers run on: its image, model, and skills.
        </p>
        {builtin.isPending ? (
          <SkeletonRows rows={1} columns={["minmax(12rem,1fr)"]} />
        ) : (
          <Select
            value={reviewerProfile === "" ? NO_PROFILE : reviewerProfile}
            onValueChange={onReviewerProfile}
            disabled={!automationId || setProfile.isPending}
          >
            <SelectTrigger className="w-72" aria-label="Reviewer profile">
              <SelectValue placeholder="Pick a profile" />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={NO_PROFILE}>No reviewer profile</SelectItem>
              {profiles.map((p) => (
                <SelectItem key={p.id} value={p.id}>
                  {p.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
        {automation && reviewerProfile === "" && (
          <div
            role="alert"
            className="flex items-start gap-2 rounded-lg border border-instrument-caution/40 bg-instrument-caution/10 p-3 text-sm"
          >
            <AlertTriangle className="mt-0.5 size-4 shrink-0 text-instrument-caution" aria-hidden />
            <span>No reviewer profile is picked, so no pull request is reviewed. Pick one.</span>
          </div>
        )}
      </section>

      {error && (
        <EmptyState tone="error">Could not load repositories — {errorMessage(error)}</EmptyState>
      )}

      {isPending ? (
        <SkeletonRows rows={3} columns={["minmax(12rem,1fr)", "6rem", "6rem", "6rem"]} />
      ) : rows.length === 0 ? (
        <EmptyState>
          No repos enrolled yet. Enroll one to have engrams review its pull requests.
        </EmptyState>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Repository</TableHead>
              <TableHead>Trigger</TableHead>
              <TableHead>Autofix</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((row) => (
              <EnrollmentRow key={row.repo} row={row} automationId={automationId!} />
            ))}
          </TableBody>
        </Table>
      )}
    </div>
  );
}

const NO_PROFILE = "__none__";

/** One entry of the automation's `repos` map, as the page shows it. */
export interface RepoRow {
  repo: string;
  mode: "auto" | "on_request";
  autofix: boolean;
}

/** The automation's `repos` input as rows, sorted by repo. A malformed
 * entry reads as the declared defaults (on request, no autofix). */
export function reposOf(inputsJson: string | undefined): RepoRow[] {
  if (!inputsJson) return [];
  try {
    const parsed: unknown = JSON.parse(inputsJson);
    const repos =
      typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)
        ? (parsed as Record<string, unknown>)["repos"]
        : undefined;
    if (typeof repos !== "object" || repos === null || Array.isArray(repos)) return [];
    return Object.entries(repos as Record<string, unknown>)
      .map(([repo, value]) => {
        const entry =
          typeof value === "object" && value !== null && !Array.isArray(value)
            ? (value as Record<string, unknown>)
            : {};
        return {
          repo,
          mode: entry["mode"] === "auto" ? ("auto" as const) : ("on_request" as const),
          autofix: entry["autofix"] === true,
        };
      })
      .sort((a, b) => a.repo.localeCompare(b.repo));
  } catch {
    return [];
  }
}

/** The automation's `profile` input: a profile id, or "" when nobody picked. */
function profileOf(inputsJson: string | undefined): string {
  if (!inputsJson) return "";
  try {
    const parsed: unknown = JSON.parse(inputsJson);
    const value =
      typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)
        ? (parsed as Record<string, unknown>)["profile"]
        : undefined;
    return typeof value === "string" ? value : "";
  } catch {
    return "";
  }
}

function TriggerBadge({ mode }: { mode: string }) {
  return mode === "auto" ? (
    <Badge variant="outline">auto</Badge>
  ) : (
    <Badge variant="secondary">@mention</Badge>
  );
}

function EnrollmentRow({ row, automationId }: { row: RepoRow; automationId: string }) {
  const remove = useSetMapInputEntry();
  return (
    <TableRow>
      <TableCell className="font-mono text-xs">{row.repo}</TableCell>
      <TableCell>
        <TriggerBadge mode={row.mode} />
      </TableCell>
      <TableCell className="text-xs text-muted-foreground">{row.autofix ? "on" : "off"}</TableCell>
      <TableCell>
        <div className="flex items-center justify-end gap-1">
          <EnrollDialog automationId={automationId} existing={row} />
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="ghost" size="sm" disabled={remove.isPending}>
                Remove
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Un-enroll {row.repo}?</AlertDialogTitle>
                <AlertDialogDescription>
                  engrams will stop reviewing this repo’s pull requests. Existing review records are
                  kept. You can re-enroll it any time.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction
                  onClick={() =>
                    remove.mutate({
                      automationId,
                      inputKey: "repos",
                      entryKey: row.repo,
                      enable: false,
                    })
                  }
                >
                  Remove
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {remove.error && (
          <EmptyState tone="error" inline className="mt-1 items-end text-right">
            Could not remove — {errorMessage(remove.error)}
          </EmptyState>
        )}
      </TableCell>
    </TableRow>
  );
}

const enrollSchema = z.object({
  repo: z
    .string()
    .trim()
    .regex(/^[^/\s]+\/[^/\s]+$/, "must be owner/name (e.g. cortexapps/engrams)"),
  mode: z.enum(["auto", "on_request"]),
  autofix: z.boolean(),
});
type EnrollValues = z.infer<typeof enrollSchema>;

function EnrollDialog({ automationId, existing }: { automationId: string; existing?: RepoRow }) {
  const [open, setOpen] = useState(false);
  const upsert = useSetMapInputEntry();
  const isEdit = existing !== undefined;

  const defaults: EnrollValues = {
    repo: existing?.repo ?? "",
    mode: existing?.mode ?? "on_request",
    autofix: existing?.autofix ?? false,
  };

  const form = useForm<EnrollValues>({
    resolver: zodResolver(enrollSchema),
    defaultValues: defaults,
  });

  const onSubmit = async (data: EnrollValues) => {
    try {
      // One entry, atomically. Enrolling a repo turns the automation on (the
      // first repo turns reviewing on); editing one never does — an admin
      // who paused reviews from the automation's page must find them still
      // paused after changing a repo's trigger. A stale snapshot of the map
      // never rides along.
      await upsert.mutateAsync({
        automationId,
        inputKey: "repos",
        entryKey: data.repo.trim(),
        valueJson: JSON.stringify({ mode: data.mode, autofix: data.autofix }),
        enable: !isEdit,
      });
      setOpen(false);
    } catch (e) {
      form.setError("root", { message: errorMessage(e) });
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (o) form.reset(defaults);
      }}
    >
      <DialogTrigger asChild>
        {isEdit ? (
          <Button variant="ghost" size="sm">
            Edit
          </Button>
        ) : (
          <Button>Enroll repo</Button>
        )}
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{isEdit ? `Edit ${existing.repo}` : "Enroll a repository"}</DialogTitle>
          <DialogDescription>
            engrams reviews pull requests on enrolled repos. The GitHub App must be installed on the
            repo for reviews to run.
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={form.handleSubmit(onSubmit)} noValidate>
          <FieldGroup>
            <Controller
              name="repo"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>Repository</FieldLabel>
                  <Input
                    {...field}
                    id={field.name}
                    autoFocus={!isEdit}
                    disabled={isEdit}
                    placeholder="cortexapps/engrams"
                    spellCheck={false}
                    autoCapitalize="off"
                    aria-invalid={fieldState.invalid}
                  />
                  <FieldDescription>
                    The GitHub owner/name. Cannot be changed later.
                  </FieldDescription>
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />
            <Controller
              name="mode"
              control={form.control}
              render={({ field }) => (
                <Field>
                  <FieldLabel htmlFor={field.name}>Trigger</FieldLabel>
                  <Select value={field.value} onValueChange={field.onChange}>
                    <SelectTrigger id={field.name} aria-label="Trigger">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="auto">Auto — review every PR when it opens</SelectItem>
                      <SelectItem value="on_request">On request — only when asked</SelectItem>
                    </SelectContent>
                  </Select>
                  <FieldDescription>
                    On-request reviews run when the app is @mentioned on the PR or dispatched.
                  </FieldDescription>
                </Field>
              )}
            />
            <Controller
              name="autofix"
              control={form.control}
              render={({ field }) => (
                <Field orientation="horizontal">
                  <Switch
                    id={field.name}
                    checked={field.value}
                    onCheckedChange={field.onChange}
                    aria-label="Autofix"
                  />
                  <FieldLabel htmlFor={field.name}>
                    Autofix — route posted findings back for fixes
                  </FieldLabel>
                </Field>
              )}
            />
            {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
          </FieldGroup>

          <DialogFooter className="mt-5">
            <Button
              type="button"
              variant="ghost"
              onClick={() => setOpen(false)}
              disabled={upsert.isPending}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={upsert.isPending}>
              {upsert.isPending ? "Saving…" : isEdit ? "Save" : "Enroll"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
