import { useState, type ReactNode } from "react";
import { toast } from "sonner";
import { PlusIcon, XIcon } from "lucide-react";
import { create } from "@bufbuild/protobuf";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useFieldArray, useForm, useWatch, type Control } from "react-hook-form";
import * as z from "zod";
import {
  useDisableImage,
  useEnableImage,
  useEnabledImages,
  useRefreshEnabledImage,
} from "../../hooks/useEnabledImages";
import { isJobActive, useEnableJobs, useRetryEnableJob } from "../../hooks/useEnableJobs";
import type { EnableJob, EnabledImageSummary } from "../../lib/types";
import { CaptureEnvEntrySchema } from "../../gen/engram/app/v1/image_pb";
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
import { Progress } from "@/components/ui/progress";
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
        description="OCI URIs tasks may reference. The manifest is cached on enable; refresh when tags move."
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
        <p className="text-sm text-destructive">
          could not load enabled images — {errorMessage(error)}
        </p>
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
              <TableHead>Capture env</TableHead>
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

  const onDisable = async (imageUri: string) => {
    try {
      await del.mutateAsync({ imageUri });
      toast.success("Image disabled");
    } catch (e) {
      // errorMessage() carries the orchestrator profile-guard text
      // ("Can't disable — N profiles use this image: …", ADR §3) or the
      // coordinator's own image_in_use text — surfaced verbatim (rawMessage,
      // no [code] prefix).
      toast.error(errorMessage(e));
    }
  };

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
      <TableCell className="text-xs">
        <CaptureEnvCell entries={row.capture_env} />
      </TableCell>
      <TableCell
        className="font-mono text-xs text-muted-foreground"
        title={new Date(row.last_refreshed_at).toLocaleString()}
      >
        {timeAgo(row.last_refreshed_at)}
      </TableCell>
      <TableCell>
        <div className="flex items-center justify-end gap-2">
          <EnableImageDialog
            editImage={row}
            trigger={
              <Button variant="ghost" size="sm">
                Edit capture env
              </Button>
            }
          />
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
                <AlertDialogAction onClick={() => onDisable(row.image_uri)}>
                  Disable image
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {refresh.error && (
          <p className="mt-1 text-right text-xs text-destructive">
            could not refresh — {errorMessage(refresh.error)}
          </p>
        )}
        {del.error && (
          <p className="mt-1 text-right text-xs text-destructive">
            could not disable — {errorMessage(del.error)}
          </p>
        )}
      </TableCell>
    </TableRow>
  );
}

// Compact read-only rendering of an enabled image's capture_env in the
// table. Secret refs are NOT secret values — the ref string is shown
// plainly (truncated, with the full value in a title tooltip).
function CaptureEnvCell({ entries }: { entries: EnabledImageSummary["capture_env"] }) {
  if (entries.length === 0) {
    return <span className="text-muted-foreground">—</span>;
  }
  return (
    <div className="max-w-[16rem] space-y-0.5">
      {entries.map((v) => (
        <div key={v.name} className="flex items-center gap-1.5">
          <code className="font-mono whitespace-nowrap">{v.name}</code>
          <Badge variant="outline" className="text-[0.6rem]">
            {v.kind === "secret_ref" ? "ref" : "literal"}
          </Badge>
          <span className="truncate text-muted-foreground" title={v.value}>
            {v.value}
          </span>
        </div>
      ))}
    </div>
  );
}

const captureEnvRowSchema = z.object({
  name: z.string(),
  kind: z.enum(["literal", "secret_ref"]),
  value: z.string(),
});
const enableImageSchema = z.object({
  imageUri: z.string().trim().min(1, "image URI is required"),
  captureEnv: z.array(captureEnvRowSchema),
});
type EnableImageValues = z.infer<typeof enableImageSchema>;

function captureEnvDefaults(image?: EnabledImageSummary): EnableImageValues["captureEnv"] {
  return (image?.capture_env ?? []).map((v) => ({
    name: v.name,
    kind: v.kind,
    value: v.value,
  }));
}

// One editable capture-env row: name · type toggle · value · remove. The
// value placeholder follows the selected type (literal vs secret ref).
function CaptureEnvRow({
  control,
  index,
  onRemove,
}: {
  control: Control<EnableImageValues>;
  index: number;
  onRemove: () => void;
}) {
  const kind = useWatch({ control, name: `captureEnv.${index}.kind` });
  return (
    <div className="flex items-start gap-2">
      <Controller
        control={control}
        name={`captureEnv.${index}.name`}
        render={({ field }) => (
          <Input
            {...field}
            className="font-mono"
            placeholder="NAME"
            aria-label="Variable name"
            spellCheck={false}
            autoCapitalize="off"
          />
        )}
      />
      <Controller
        control={control}
        name={`captureEnv.${index}.kind`}
        render={({ field }) => (
          <Select value={field.value} onValueChange={field.onChange}>
            <SelectTrigger size="sm" className="w-32 shrink-0" aria-label="Variable type">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="literal">Literal</SelectItem>
              <SelectItem value="secret_ref">Secret ref</SelectItem>
            </SelectContent>
          </Select>
        )}
      />
      <Controller
        control={control}
        name={`captureEnv.${index}.value`}
        render={({ field }) => (
          <Input
            {...field}
            className="font-mono"
            placeholder={
              kind === "secret_ref" ? "gcp-sm://…/secrets/foo/versions/latest" : "literal value"
            }
            aria-label="Variable value"
            spellCheck={false}
            autoCapitalize="off"
          />
        )}
      />
      <Button
        type="button"
        variant="ghost"
        size="icon-sm"
        className="shrink-0"
        onClick={onRemove}
        aria-label="Remove variable"
      >
        <XIcon />
      </Button>
    </div>
  );
}

// Enable a new image, or edit an already-enabled image's capture_env. In
// edit mode the URI is pinned (read-only) and the form pre-fills with the
// row's current capture_env — re-submitting re-enables with the full list,
// which REPLACES the set (ADR 0057). There is no separate update RPC.
function EnableImageDialog({
  editImage,
  trigger,
}: {
  editImage?: EnabledImageSummary;
  trigger?: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const enable = useEnableImage();
  const isEdit = !!editImage;

  const form = useForm<EnableImageValues>({
    resolver: zodResolver(enableImageSchema),
    defaultValues: {
      imageUri: editImage?.image_uri ?? "",
      captureEnv: captureEnvDefaults(editImage),
    },
  });
  const { fields, append, remove } = useFieldArray({
    control: form.control,
    name: "captureEnv",
  });

  // Re-seed on every open so an edit always reflects the row's current
  // capture_env and a cancelled edit doesn't linger in the form.
  const onOpenChange = (next: boolean) => {
    setOpen(next);
    if (next) {
      form.reset({
        imageUri: editImage?.image_uri ?? "",
        captureEnv: captureEnvDefaults(editImage),
      });
    }
  };

  const onSubmit = async (data: EnableImageValues) => {
    // Skip rows with an empty name; map each surviving row's type toggle to
    // the proto oneof. A non-empty list REPLACES the image's capture_env;
    // an empty list on a plain re-enable inherits the existing set.
    const captureEnv = data.captureEnv
      .filter((r) => r.name.trim() !== "")
      .map((r) =>
        create(CaptureEnvEntrySchema, {
          name: r.name.trim(),
          value: {
            case: r.kind === "secret_ref" ? "secretRef" : "literal",
            value: r.value,
          },
        }),
      );
    try {
      await enable.mutateAsync({ imageUri: data.imageUri, captureEnv });
      setOpen(false);
    } catch (err) {
      // Surface the coordinator's real message (e.g. a registry-auth
      // failure) instead of an opaque `[internal] HTTP 400`.
      form.setError("root", { message: errorMessage(err) });
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogTrigger asChild>{trigger ?? <Button>Enable a new image</Button>}</DialogTrigger>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{isEdit ? "Edit capture env" : "Enable a new image"}</DialogTitle>
          <DialogDescription>
            {isEdit ? (
              <>
                Re-enabling replaces this image's capture-time env with the full list below.
                Materialization progress shows in the list above.
              </>
            ) : (
              <>
                Full OCI reference:{" "}
                <code className="font-mono">&lt;host&gt;[:port]/&lt;repo&gt;:&lt;tag&gt;</code>. The
                coordinator queues an enable job — materialization progress shows in the list above.
              </>
            )}
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
                    autoFocus={!isEdit}
                    readOnly={isEdit}
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

            <Field>
              <div className="flex items-center justify-between">
                <FieldLabel>Capture-time env</FieldLabel>
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  onClick={() => append({ name: "", kind: "literal", value: "" })}
                >
                  <PlusIcon /> Add variable
                </Button>
              </div>
              <FieldDescription>
                Injected into the image's <code className="font-mono">[warm]</code> hook at
                base-snapshot capture (not a session secret). Each is a literal value or a secret
                ref (e.g. <code className="font-mono">gcp-sm://…</code>) resolved server-side.
                Leaving this empty on a re-enable keeps the current set.
              </FieldDescription>
              {fields.length === 0 ? (
                <p className="text-xs text-muted-foreground">No capture vars.</p>
              ) : (
                <div className="space-y-2">
                  {fields.map((f, i) => (
                    <CaptureEnvRow
                      key={f.id}
                      control={form.control}
                      index={i}
                      onRemove={() => remove(i)}
                    />
                  ))}
                </div>
              )}
            </Field>

            {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
          </FieldGroup>
          <DialogFooter className="mt-4 sm:items-center">
            <Button
              type="button"
              variant="ghost"
              onClick={() => onOpenChange(false)}
              disabled={enable.isPending}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={enable.isPending}>
              {enable.isPending ? (isEdit ? "Saving…" : "Enabling…") : isEdit ? "Save" : "Enable"}
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
// job through pending → materializing → capturing → prestaging → ready
// (ADR 0036 amendment / issue #538 added "prestaging"), updating
// chunks_done/chunks_total as it materializes. We render a thin bar +
// the state label; failed jobs keep their error visible with a retry
// affordance.

const JOB_STATE_LABEL: Record<EnableJob["state"], string> = {
  pending: "queued",
  materializing: "materializing chunks",
  capturing: "capturing canonical snapshot",
  // ADR 0036 amendment (issue #538): the fleet chunk-prestage wait — every
  // eligible host warms the base snapshot before the image is usable.
  prestaging: "staging chunks to hosts",
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
