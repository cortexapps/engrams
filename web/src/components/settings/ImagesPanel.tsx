import { useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import {
  useDisableImage,
  useEnableImage,
  useEnabledImages,
  useRefreshEnabledImage,
} from "../../hooks/useEnabledImages";
import { isJobActive, useEnableJobs, useRetryEnableJob } from "../../hooks/useEnableJobs";
import type { EnableJob, EnabledImageSummary } from "../../types";
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
import { Field, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Progress } from "@/components/ui/progress";
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

// Enabled-images panel — Stage D + ADR 0036. Operators curate the OCI URIs
// sessions may reference. The list reads `GET /api/enabled-images`
// (Postgres-backed); enabling POSTs a job (202) and the panel polls
// `GET /api/enable-jobs` to render REAL pipeline progress
// (materializing chunks → capturing snapshot → ready), replacing the old
// wall-clock-driven stage guesser.
export function ImagesPanel() {
  const { data, isLoading, error } = useEnabledImages();
  const rows = data ?? [];
  const { data: jobs } = useEnableJobs();

  // ADR 0036: in-flight enables (and fresh failures, kept visible
  // for an hour so the error + retry affordance doesn't vanish).
  const visibleJobs = (jobs ?? []).filter(
    (j) =>
      isJobActive(j) ||
      (j.state === "failed" && Date.now() - new Date(j.updated_at).getTime() < 60 * 60 * 1000),
  );

  // #94: for any URI that has a visible job, the job row takes
  // precedence — drop the matching image row so a refresh shows a
  // single in-flight row instead of duplicating (job card + table row).
  const jobUris = new Set(visibleJobs.map((j) => j.image_uri));
  const imageRows = rows.filter((r) => !jobUris.has(r.image_uri));

  return (
    <div className="space-y-6">
      <PageHeading
        title="Images"
        description="OCI URIs sessions may reference. The manifest is cached on enable; refresh when tags move."
        actions={<EnableImageDialog />}
      />

      {visibleJobs.length > 0 && (
        <ul className="space-y-2 mb-6">
          {visibleJobs.map((job) => (
            <EnableJobRow key={job.id} job={job} />
          ))}
        </ul>
      )}

      {error && (
        <p className="text-sm text-destructive">could not load enabled images — {String(error)}</p>
      )}

      {isLoading ? (
        <p className="py-6 text-sm text-muted-foreground">Loading…</p>
      ) : visibleJobs.length === 0 && imageRows.length === 0 ? (
        <Card>
          <CardContent className="py-10 text-center text-sm text-muted-foreground">
            No images enabled. Bake + push an image with{" "}
            <code className="font-mono">engram image build --push</code>, then enable its URI here.
          </CardContent>
        </Card>
      ) : imageRows.length > 0 ? (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Image</TableHead>
              <TableHead>Manifest</TableHead>
              <TableHead>Digest</TableHead>
              <TableHead>Refreshed</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {imageRows.map((row) => (
              <ImageRow key={row.id} row={row} />
            ))}
          </TableBody>
        </Table>
      ) : null}
    </div>
  );
}

function ImageRow({ row }: { row: EnabledImageSummary }) {
  const del = useDisableImage();
  const refresh = useRefreshEnabledImage();
  const shortDigest =
    row.manifest_digest.length > 19 ? `${row.manifest_digest.slice(0, 19)}…` : row.manifest_digest;

  return (
    <TableRow>
      <TableCell className="font-mono text-sm whitespace-nowrap">{row.image_uri}</TableCell>
      <TableCell className="text-sm text-muted-foreground whitespace-normal">
        <div className="max-w-md">
          {row.manifest_name || "—"}
          {row.manifest_description && (
            <span className="mt-0.5 block text-xs line-clamp-2" title={row.manifest_description}>
              {row.manifest_description}
            </span>
          )}
        </div>
      </TableCell>
      <TableCell>
        <Badge variant="outline" className="font-mono text-[0.65rem]" title={row.manifest_digest}>
          {shortDigest}
        </Badge>
      </TableCell>
      <TableCell
        className="font-mono text-xs text-muted-foreground"
        title={new Date(row.last_refreshed_at).toLocaleString()}
      >
        {timeAgo(row.last_refreshed_at)}
      </TableCell>
      <TableCell>
        <div className="flex items-center justify-end gap-2">
          <Button
            variant="ghost"
            size="sm"
            onClick={() => refresh.mutate({ imageUri: row.image_uri })}
            disabled={refresh.isPending}
          >
            {refresh.isPending ? "Refreshing…" : "Refresh"}
          </Button>
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="ghost" size="sm" disabled={del.isPending}>
                Disable
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Disable {row.image_uri}?</AlertDialogTitle>
                <AlertDialogDescription>
                  Sessions can no longer reference this URI. The artifact in the registry is
                  untouched.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction onClick={() => del.mutate({ imageUri: row.image_uri })}>
                  Disable image
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {refresh.error && (
          <p className="mt-1 text-right text-xs text-destructive">
            could not refresh — {String(refresh.error)}
          </p>
        )}
        {del.error && (
          <p className="mt-1 text-right text-xs text-destructive">
            could not disable — {String(del.error)}
          </p>
        )}
      </TableCell>
    </TableRow>
  );
}

const enableImageSchema = z.object({
  imageUri: z.string().trim().min(1, "image URI is required"),
});
type EnableImageValues = z.infer<typeof enableImageSchema>;

function EnableImageDialog() {
  const [open, setOpen] = useState(false);
  const enable = useEnableImage();
  const form = useForm<EnableImageValues>({
    resolver: zodResolver(enableImageSchema),
    defaultValues: { imageUri: "" },
  });

  const onSubmit = async (data: EnableImageValues) => {
    try {
      await enable.mutateAsync({ imageUri: data.imageUri });
      form.reset();
      setOpen(false);
    } catch (err) {
      form.setError("root", { message: String(err) });
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button>Enable a new image</Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Enable a new image</DialogTitle>
          <DialogDescription>
            Full OCI reference:{" "}
            <code className="font-mono">&lt;host&gt;[:port]/&lt;repo&gt;:&lt;tag&gt;</code>. The
            coordinator queues an enable job — materialization progress shows in the list above.
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={form.handleSubmit(onSubmit)}>
          <FieldGroup>
            <Controller
              name="imageUri"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>Image URI</FieldLabel>
                  <Input
                    {...field}
                    id={field.name}
                    autoFocus
                    className="font-mono"
                    placeholder="ghcr.io/cortex/api:warm-1"
                    spellCheck={false}
                    autoCapitalize="off"
                    aria-invalid={fieldState.invalid}
                  />
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />
            {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
          </FieldGroup>
          <DialogFooter className="mt-4 sm:items-center">
            <Button
              type="button"
              variant="ghost"
              onClick={() => setOpen(false)}
              disabled={enable.isPending}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={enable.isPending}>
              {enable.isPending ? "Enabling…" : "Enable"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

// ---------- Enable-job progress (ADR 0036) ---------------------------
//
// Real progress from the server: the coordinator's scanner drives the
// job through pending → materializing → capturing → ready, updating
// chunks_done/chunks_total as it materializes. We render a thin bar +
// the state label; failed jobs keep their error visible with a retry
// affordance.

const JOB_STATE_LABEL: Record<EnableJob["state"], string> = {
  pending: "queued",
  materializing: "materializing chunks",
  capturing: "capturing canonical snapshot",
  ready: "ready",
  failed: "failed",
};

function EnableJobRow({ job }: { job: EnableJob }) {
  const retry = useRetryEnableJob();
  const failed = job.state === "failed";
  const pct =
    job.chunks_total && job.chunks_total > 0
      ? Math.min(100, Math.round((job.chunks_done / job.chunks_total) * 100))
      : null;

  return (
    <li>
      <Card className={failed ? "border-destructive/40" : undefined}>
        <CardContent className="space-y-2 py-3">
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-mono text-sm whitespace-nowrap">{job.image_uri}</span>
            <Badge variant={failed ? "destructive" : "secondary"} className="font-normal">
              {JOB_STATE_LABEL[job.state]}
              {job.state === "materializing" && job.chunks_total
                ? ` · ${job.chunks_done}/${job.chunks_total} chunks`
                : ""}
            </Badge>
            {!failed && (
              <span className="text-xs text-muted-foreground italic animate-pulse">working…</span>
            )}
            {failed && (
              <Button
                variant="ghost"
                size="sm"
                className="ml-auto"
                onClick={() => retry.mutate({ jobId: job.id })}
                disabled={retry.isPending}
              >
                {retry.isPending ? "Retrying…" : "Retry"}
              </Button>
            )}
          </div>
          {pct !== null && !failed && <Progress value={pct} className="h-1 max-w-md" />}
          {failed && job.error && <p className="text-xs text-destructive">{job.error}</p>}
        </CardContent>
      </Card>
    </li>
  );
}

function timeAgo(iso: string): string {
  const then = new Date(iso).getTime();
  const now = Date.now();
  if (Number.isNaN(then)) return iso;
  const seconds = Math.max(0, Math.floor((now - then) / 1000));
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  return new Date(iso).toLocaleDateString();
}
