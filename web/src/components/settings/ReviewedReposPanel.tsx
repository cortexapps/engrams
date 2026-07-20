import { useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import { AlertTriangle } from "lucide-react";

import {
  useEnrollments,
  useUpsertEnrollment,
  useDeleteEnrollment,
} from "../../hooks/useEnrollments";
import { useProfiles } from "../../hooks/useProfiles";
import type { RepoEnrollment } from "../../gen/engram/app/v1/review_pb";
import { errorMessage } from "../../lib/errors";
import { PageHeading } from "../page-heading";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
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

// PR-review enrollment (ADR 0100). A repo must appear here for engrams to touch
// its pull requests — the GitHub webhook drops any event whose repo is not
// enrolled. `trigger_mode` decides whether a PR is reviewed automatically on
// open (auto) or only when the app is @mentioned / dispatched (manual).
// `autofix` decides whether posted findings route back for fixes. The profile
// override picks which session profile the review runs on; left as the default,
// it uses whichever profile is designated the `pr_reviewer`.
export function ReviewedReposPanel() {
  const { data, isPending, error } = useEnrollments();
  const { data: profileData } = useProfiles();
  const rows = data?.enrollments ?? [];
  const profiles = profileData?.profiles ?? [];
  const hasReviewerProfile = profiles.some((p) => p.designation === "pr_reviewer");

  return (
    <div className="space-y-6">
      <PageHeading
        title="Reviewed repos"
        eyebrow="Pull requests"
        description="Repositories engrams reviews. A repo must be enrolled here — pull-request events from repos that aren’t listed are ignored."
        actions={<EnrollDialog profiles={profiles} />}
      />

      {!hasReviewerProfile && (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-sm"
        >
          <AlertTriangle className="mt-0.5 size-4 shrink-0 text-amber-600" aria-hidden />
          <span>
            No profile is designated the <code className="font-mono">pr_reviewer</code>. Reviews on
            repos without a profile override will fail. Set the “PR reviewer” toggle on a profile
            under Settings → Profiles.
          </span>
        </div>
      )}

      {error && (
        <p className="text-sm text-destructive">
          could not load enrollments — {errorMessage(error)}
        </p>
      )}

      {isPending ? (
        <p className="py-6 text-sm text-muted-foreground">Loading…</p>
      ) : rows.length === 0 ? (
        <Card>
          <CardContent className="py-10 text-center text-sm text-muted-foreground">
            No repos enrolled yet. Enroll one to have engrams review its pull requests.
          </CardContent>
        </Card>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Repository</TableHead>
              <TableHead>Trigger</TableHead>
              <TableHead>Autofix</TableHead>
              <TableHead>Profile</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((row) => (
              <EnrollmentRow key={row.repo} row={row} profiles={profiles} />
            ))}
          </TableBody>
        </Table>
      )}
    </div>
  );
}

type ProfileLite = { id: string; name: string; designation?: string };

function TriggerBadge({ mode }: { mode: string }) {
  return mode === "auto" ? <Badge>auto</Badge> : <Badge variant="secondary">@mention</Badge>;
}

function profileLabel(row: RepoEnrollment, profiles: ProfileLite[]): string {
  if (!row.profileId) return "Default (pr_reviewer)";
  return profiles.find((p) => p.id === row.profileId)?.name ?? row.profileId;
}

function EnrollmentRow({ row, profiles }: { row: RepoEnrollment; profiles: ProfileLite[] }) {
  const remove = useDeleteEnrollment();
  return (
    <TableRow>
      <TableCell className="font-mono text-xs">{row.repo}</TableCell>
      <TableCell>
        <TriggerBadge mode={row.triggerMode} />
      </TableCell>
      <TableCell className="text-xs text-muted-foreground">{row.autofix}</TableCell>
      <TableCell className="text-xs text-muted-foreground">{profileLabel(row, profiles)}</TableCell>
      <TableCell>
        <div className="flex items-center justify-end gap-1">
          <EnrollDialog profiles={profiles} existing={row} />
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
                <AlertDialogAction onClick={() => remove.mutate({ repo: row.repo })}>
                  Remove
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {remove.error && (
          <p className="mt-1 text-right text-xs text-destructive">
            could not remove — {errorMessage(remove.error)}
          </p>
        )}
      </TableCell>
    </TableRow>
  );
}

// Empty profile override = fall back to the designated pr_reviewer profile. We
// represent that sentinel with a non-empty token because the shadcn Select
// disallows an empty-string value.
const DEFAULT_PROFILE = "__default__";

const enrollSchema = z.object({
  repo: z
    .string()
    .trim()
    .regex(/^[^/\s]+\/[^/\s]+$/, "must be owner/name (e.g. cortexapps/engrams)"),
  triggerMode: z.enum(["auto", "manual"]),
  autofix: z.enum(["auto", "manual", "off"]),
  profileId: z.string(),
});
type EnrollValues = z.infer<typeof enrollSchema>;

function EnrollDialog({
  profiles,
  existing,
}: {
  profiles: ProfileLite[];
  existing?: RepoEnrollment;
}) {
  const [open, setOpen] = useState(false);
  const upsert = useUpsertEnrollment();
  const isEdit = existing !== undefined;

  const defaults: EnrollValues = {
    repo: existing?.repo ?? "",
    triggerMode: (existing?.triggerMode as "auto" | "manual") ?? "manual",
    autofix: (existing?.autofix as "auto" | "manual" | "off") ?? "off",
    profileId: existing?.profileId ? existing.profileId : DEFAULT_PROFILE,
  };

  const form = useForm<EnrollValues>({
    resolver: zodResolver(enrollSchema),
    defaultValues: defaults,
  });

  const onSubmit = async (data: EnrollValues) => {
    try {
      await upsert.mutateAsync({
        repo: data.repo.trim(),
        triggerMode: data.triggerMode,
        autofix: data.autofix,
        profileId: data.profileId === DEFAULT_PROFILE ? "" : data.profileId,
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
              name="triggerMode"
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
                      <SelectItem value="manual">On @mention — only when asked</SelectItem>
                    </SelectContent>
                  </Select>
                  <FieldDescription>
                    Manual reviews run when the app is @mentioned on the PR or dispatched.
                  </FieldDescription>
                </Field>
              )}
            />
            <Controller
              name="autofix"
              control={form.control}
              render={({ field }) => (
                <Field>
                  <FieldLabel htmlFor={field.name}>Autofix</FieldLabel>
                  <Select value={field.value} onValueChange={field.onChange}>
                    <SelectTrigger id={field.name} aria-label="Autofix">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="off">Off — post findings only</SelectItem>
                      <SelectItem value="manual">Manual — offer fixes on request</SelectItem>
                      <SelectItem value="auto">Auto — route findings back for fixes</SelectItem>
                    </SelectContent>
                  </Select>
                </Field>
              )}
            />
            <Controller
              name="profileId"
              control={form.control}
              render={({ field }) => (
                <Field>
                  <FieldLabel htmlFor={field.name}>Profile</FieldLabel>
                  <Select value={field.value} onValueChange={field.onChange}>
                    <SelectTrigger id={field.name} aria-label="Profile">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value={DEFAULT_PROFILE}>
                        Default (designated pr_reviewer)
                      </SelectItem>
                      {profiles.map((p) => (
                        <SelectItem key={p.id} value={p.id}>
                          {p.name}
                          {p.designation === "pr_reviewer" ? " (pr_reviewer)" : ""}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <FieldDescription>
                    Which session profile the review runs on. Leave default to use the designated
                    reviewer profile.
                  </FieldDescription>
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
