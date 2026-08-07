import { useState, type ReactNode } from "react";
import { toast } from "sonner";
import { PlusIcon, XIcon } from "lucide-react";
import { create, equals } from "@bufbuild/protobuf";
import { ConnectError, Code } from "@connectrpc/connect";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useFieldArray, useForm, useWatch, type Control } from "react-hook-form";
import * as z from "zod";
import {
  useDisableImage,
  useEnableImage,
  useEnabledImages,
  useRefreshEnabledImage,
  useUpdateImage,
} from "../../hooks/useEnabledImages";
import { isJobActive, useEnableJobs, useRetryEnableJob } from "../../hooks/useEnableJobs";
import { useOrgSecretNames } from "../../hooks/useOrgSecrets";
import type { EnableJob, EnabledImageSummary, StageRecord } from "../../lib/types";
import {
  CaptureEnvEntrySchema,
  ImageConfigSchema,
  ImageResourcesSchema,
  ImageWarmConfigSchema,
  type ImageConfig,
} from "../../gen/engram/app/v1/image_pb";
import { ProfileNetworkSchema } from "../../gen/engram/app/v1/profile_pb";
import { errorMessage } from "../../lib/errors";
import { PageHeading } from "../page-heading";
import { OrgSecretCombobox } from "../integrations/OrgSecretCombobox";
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
import { Textarea } from "@/components/ui/textarea";
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
      <PageHeading title="Images" actions={<EnableImageDialog />} />

      {visibleJobs.length > 0 && (
        <ul className="space-y-2 mb-6">
          {visibleJobs.map((job) => (
            <EnableJobRow
              key={job.id}
              job={job}
              // ETA reference: the most recent READY job for the same URI —
              // its stage durations are the "typically ~Xm" denominators.
              reference={(jobs ?? []).find(
                (j) => j.state === "ready" && j.image_uri === job.image_uri,
              )}
            />
          ))}
        </ul>
      )}

      {error && (
        <p className="text-sm text-destructive">
          Could not load enabled images — {errorMessage(error)}
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
              <TableHead>Name</TableHead>
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
          {row.name || "—"}
          {row.description && (
            <span className="mt-0.5 block text-xs line-clamp-2" title={row.description}>
              {row.description}
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
                Edit config
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
            Could not refresh — {errorMessage(refresh.error)}
          </p>
        )}
        {del.error && (
          <p className="mt-1 text-right text-xs text-destructive">
            Could not disable — {errorMessage(del.error)}
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
// ADR 0080: the form collects the full ImageConfig. Numeric fields stay
// strings in form state (native inputs yield strings); onSubmit converts.
const enableImageSchema = z
  .object({
    imageUri: z.string().trim().min(1, "image URI is required"),
    name: z.string().trim().min(1, "name is required"),
    description: z.string(),
    envRows: z.array(z.object({ key: z.string(), value: z.string() })),
    workdir: z.string(),
    vcpus: z
      .string()
      .trim()
      .regex(/^[1-9]\d*$/, "vCPUs must be a positive integer"),
    memoryMib: z
      .string()
      .trim()
      .regex(/^(?:[1-9]\d*)?$/, "memory must be a positive integer (MiB)"),
    diskGib: z
      .string()
      .trim()
      .regex(/^(?:[1-9]\d*)?$/, "disk must be a positive integer (GiB)"),
    swapMib: z
      .string()
      .trim()
      .regex(/^(?:[1-9]\d*)?$/, "swap must be a positive integer (MiB)"),
    warmCommand: z.string(),
    warmTimeoutSecs: z
      .string()
      .trim()
      .regex(/^(?:[1-9]\d*)?$/, "timeout must be a positive integer (seconds)"),
    warmWorkdir: z.string(),
    captureEnv: z.array(captureEnvRowSchema),
    // "none" = no warm.network at all (egress-less capture); "deny"/"allow"
    // map to ProfileNetwork.default. Sent only when a warm command is set.
    netDefault: z.enum(["none", "deny", "allow"]),
    allowHostsText: z.string(),
    allowPatternsText: z.string(),
  })
  .refine((v) => v.warmCommand.trim() !== "" || v.captureEnv.every((r) => r.name.trim() === ""), {
    message: "warm env needs a warm command — everything warm rides the [warm] block",
    path: ["warmCommand"],
  });
type EnableImageValues = z.infer<typeof enableImageSchema>;

// Pre-fill the full-config form from the enabled row (edit mode) or with
// blank/default values (first enable). Every ImageConfig field must seed
// from the row so an untouched re-submit reproduces the row's config
// exactly (the server then treats it as a cheap no-op edit, not a
// recapture) — see buildConfig for the inverse.
function formDefaults(image?: EnabledImageSummary): EnableImageValues {
  return {
    imageUri: image?.image_uri ?? "",
    name: image?.name ?? "",
    description: image?.description ?? "",
    envRows: Object.entries(image?.env ?? {}).map(([key, value]) => ({ key, value })),
    workdir: image?.workdir ?? "",
    vcpus: image?.suggested_vcpus != null ? String(image.suggested_vcpus) : "2",
    memoryMib: image?.suggested_memory_mib != null ? String(image.suggested_memory_mib) : "",
    diskGib: image?.suggested_disk_gib != null ? String(image.suggested_disk_gib) : "",
    swapMib: image?.suggested_swap_mib != null ? String(image.suggested_swap_mib) : "",
    warmCommand: (image?.warm_command ?? []).join(" "),
    warmTimeoutSecs: image?.warm_timeout_secs != null ? String(image.warm_timeout_secs) : "",
    warmWorkdir: image?.warm_workdir ?? "",
    captureEnv: (image?.capture_env ?? []).map((v) => ({
      name: v.name,
      kind: v.kind,
      value: v.value,
    })),
    netDefault: image?.warm_network ? image.warm_network.default : "none",
    allowHostsText: (image?.warm_network?.allow_hosts ?? []).join("\n"),
    allowPatternsText: (image?.warm_network?.allow_host_patterns ?? []).join("\n"),
  };
}

const linesOf = (text: string) =>
  text
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);

// Form values → the full proto ImageConfig (ADR 0080: always the whole
// config; the server replaces wholesale). The inverse of formDefaults:
// empty-string optionals stay ABSENT (undefined), never become "" — the
// round-trip invariant the cheap-edit path depends on.
function buildConfig(data: EnableImageValues): ImageConfig {
  const env: Record<string, string> = {};
  // Last write wins on a duplicate key, matching the profile env editor.
  for (const r of data.envRows) if (r.key.trim()) env[r.key.trim()] = r.value;
  // Skip env rows with an empty name; map each surviving row's type toggle
  // to the proto oneof.
  const warmEnv = data.captureEnv
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
  const warmCommand = data.warmCommand.trim();
  return create(ImageConfigSchema, {
    name: data.name,
    description: data.description.trim() || undefined,
    env,
    workdir: data.workdir.trim() || undefined,
    resources: create(ImageResourcesSchema, {
      suggestedVcpus: Number(data.vcpus),
      suggestedMemoryMib: data.memoryMib ? Number(data.memoryMib) : undefined,
      suggestedDiskGib: data.diskGib ? Number(data.diskGib) : undefined,
      suggestedSwapMib: data.swapMib ? Number(data.swapMib) : undefined,
    }),
    // Empty command = no [warm] hook (the schema already rejects warm env
    // without a command; timeout/workdir/network ride the block too).
    warm: warmCommand
      ? create(ImageWarmConfigSchema, {
          command: warmCommand.split(/\s+/),
          timeoutSecs: data.warmTimeoutSecs ? BigInt(data.warmTimeoutSecs) : undefined,
          workdir: data.warmWorkdir.trim() || undefined,
          env: warmEnv,
          // "none" = unset = egress-less capture (ADR 0080).
          network:
            data.netDefault !== "none"
              ? create(ProfileNetworkSchema, {
                  default: data.netDefault,
                  allowHosts: linesOf(data.allowHostsText),
                  allowHostPatterns: linesOf(data.allowPatternsText),
                })
              : undefined,
        })
      : undefined,
  });
}

// Capture-affecting edits are intentionally a two-step operation: unlike
// name/env/workdir changes, resources and warm are baked into the base
// snapshot and require the minutes-long enable pipeline to run again. Detect
// the diff before the first request so confirmation is normal UI flow, while
// the coordinator's allow_recapture gate remains the authoritative backstop.
function recaptureFields(image: EnabledImageSummary, next: ImageConfig): string[] {
  const current = buildConfig(formDefaults(image));
  const fields: string[] = [];

  const resourcesChanged =
    current.resources && next.resources
      ? !equals(ImageResourcesSchema, current.resources, next.resources)
      : current.resources !== next.resources;
  if (resourcesChanged) fields.push("resources");

  const warmChanged =
    current.warm && next.warm
      ? !equals(ImageWarmConfigSchema, current.warm, next.warm)
      : current.warm !== next.warm;
  if (warmChanged) fields.push("warm");

  return fields;
}

// One editable capture-env row: name · type toggle · value · remove. A
// secret_ref row picks an org-secret name via the same typeahead the
// profile secrets editor uses (only names cross the wire, never values);
// a literal row keeps the plain input.
function CaptureEnvRow({
  control,
  index,
  secretNames,
  onRemove,
}: {
  control: Control<EnableImageValues>;
  index: number;
  /** Existing org-secret names for the secret-ref typeahead. */
  secretNames: string[];
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
        render={({ field }) =>
          kind === "secret_ref" ? (
            <OrgSecretCombobox
              value={field.value}
              onChange={field.onChange}
              secretNames={secretNames}
              placeholder="org-secret name (ref)"
              className="min-w-0 flex-1"
            />
          ) : (
            <Input
              {...field}
              className="font-mono"
              placeholder="literal value"
              aria-label="Variable value"
              spellCheck={false}
              autoCapitalize="off"
            />
          )
        }
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

// One editable image-env row (config.env): key · value · remove. Mirrors
// the CaptureEnvRow visual style minus the type toggle — image env is
// always a plain non-secret literal (merged over Dockerfile ENV, under
// session env).
function ImageEnvRow({
  control,
  index,
  onRemove,
}: {
  control: Control<EnableImageValues>;
  index: number;
  onRemove: () => void;
}) {
  return (
    <div className="flex items-start gap-2">
      <Controller
        control={control}
        name={`envRows.${index}.key`}
        render={({ field }) => (
          <Input
            {...field}
            className="font-mono"
            placeholder="KEY"
            aria-label="Env key"
            spellCheck={false}
            autoCapitalize="off"
          />
        )}
      />
      <Controller
        control={control}
        name={`envRows.${index}.value`}
        render={({ field }) => (
          <Input
            {...field}
            className="font-mono"
            placeholder="value"
            aria-label="Env value"
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
        aria-label="Remove env var"
      >
        <XIcon />
      </Button>
    </div>
  );
}

// Enable a new image, or edit an already-enabled image's config. In edit
// mode the URI is pinned (read-only), the form pre-fills with the row's
// current config, and saving goes through UpdateImage (ADR 0080 phase 2b):
// cheap fields (name/description/env/workdir) are sent immediately with
// allow_recapture=false. A local diff touching resources or warm first opens
// an inline confirmation block; confirming sends allow_recapture=true and
// spawns a recapture job. The server's FailedPrecondition remains a fallback
// for stale/concurrent edits the local comparison could not anticipate.
// Create mode keeps EnableImage, which queues the initial enable job.
function EnableImageDialog({
  editImage,
  trigger,
}: {
  editImage?: EnabledImageSummary;
  trigger?: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  // A capture-affecting edit awaiting operator confirmation: the config
  // built at submit time + the server's FailedPrecondition text (it names
  // the offending fields).
  const [pendingRecapture, setPendingRecapture] = useState<{
    config: ImageConfig;
    reason: string;
  } | null>(null);
  const enable = useEnableImage();
  const update = useUpdateImage();
  const { data: orgSecretNames } = useOrgSecretNames();
  const isEdit = !!editImage;
  const busy = enable.isPending || update.isPending;

  const form = useForm<EnableImageValues>({
    resolver: zodResolver(enableImageSchema),
    defaultValues: formDefaults(editImage),
  });
  const { fields, append, remove } = useFieldArray({
    control: form.control,
    name: "captureEnv",
  });
  const envArray = useFieldArray({ control: form.control, name: "envRows" });
  // The warm network editor is only meaningful with a warm command (the
  // policy applies to the capture VM while the hook runs) — hide it (and
  // disable the other warm extras) otherwise.
  const warmCommandLive = useWatch({ control: form.control, name: "warmCommand" });
  const hasWarmCommand = warmCommandLive.trim() !== "";
  const netDefaultLive = useWatch({ control: form.control, name: "netDefault" });

  // Re-seed on every open so an edit always reflects the row's current
  // config and a cancelled edit doesn't linger in the form.
  const onOpenChange = (next: boolean) => {
    setOpen(next);
    setPendingRecapture(null);
    if (next) {
      form.reset(formDefaults(editImage));
    }
  };

  const onSubmit = async (data: EnableImageValues) => {
    // The config is ALWAYS sent whole (ADR 0080): the form is the full
    // ImageConfig, so a submit replaces the row's config wholesale (an edit
    // pre-fills from the row, so an untouched re-submit round-trips as a
    // no-op).
    const config = buildConfig(data);
    if (!isEdit) {
      try {
        await enable.mutateAsync({ imageUri: data.imageUri, config });
        setOpen(false);
      } catch (err) {
        // Surface the coordinator's real message (e.g. a registry-auth
        // failure) instead of an opaque `[internal] HTTP 400`.
        form.setError("root", { message: errorMessage(err) });
      }
      return;
    }
    const fields = recaptureFields(editImage, config);
    if (fields.length > 0) {
      setPendingRecapture({
        config,
        reason: `This edit changes ${fields.join(" and ")} — applying it requires a base-snapshot recapture.`,
      });
      return;
    }
    // Edit: optimistically try the cheap path. The server accepts a diff
    // confined to name/description/env/workdir outright; a capture-affecting
    // diff (resources, anything under warm) fails FailedPrecondition naming
    // the fields — surface that as an explicit recapture confirmation
    // instead of silently kicking off a minutes-long job (ADR 0080).
    try {
      await update.mutateAsync({ imageUri: data.imageUri, config, allowRecapture: false });
      toast.success("Config updated — changes applied immediately");
      setOpen(false);
    } catch (err) {
      const ce = ConnectError.from(err);
      if (ce.code === Code.FailedPrecondition) {
        setPendingRecapture({ config, reason: ce.rawMessage });
      } else {
        form.setError("root", { message: errorMessage(err) });
      }
    }
  };

  const onConfirmRecapture = async () => {
    if (!pendingRecapture || !editImage) return;
    try {
      await update.mutateAsync({
        imageUri: editImage.image_uri,
        config: pendingRecapture.config,
        allowRecapture: true,
      });
      // The returned job lands in the enable-jobs table via the hook's
      // invalidation — no extra wiring here.
      toast.success("Recapture started — sessions keep the old snapshot until it's ready");
      setPendingRecapture(null);
      setOpen(false);
    } catch (err) {
      setPendingRecapture(null);
      form.setError("root", { message: errorMessage(err) });
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogTrigger asChild>{trigger ?? <Button>Enable a new image</Button>}</DialogTrigger>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{isEdit ? "Edit config" : "Enable a new image"}</DialogTitle>
          <DialogDescription>
            {isEdit ? (
              <>
                Full-replace edit (ADR 0080): name, description, image env and workdir apply
                immediately; a change to resources or the warm hook asks for confirmation, then
                recaptures the base snapshot.
              </>
            ) : (
              <>
                Full OCI reference:{" "}
                <code className="font-mono">&lt;host&gt;[:port]/&lt;repo&gt;:&lt;tag&gt;</code>. The
                config below is applied at enable time (ADR 0080). The coordinator queues an enable
                job — materialization progress shows in the list above.
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

            <Controller
              name="name"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>Name</FieldLabel>
                  <Input
                    {...field}
                    id={field.name}
                    placeholder="cortex-api"
                    aria-invalid={fieldState.invalid}
                  />
                  <FieldDescription>Display name shown in pickers.</FieldDescription>
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />

            <Controller
              name="description"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>Description (optional)</FieldLabel>
                  <Input
                    {...field}
                    id={field.name}
                    placeholder="What this image is for"
                    aria-invalid={fieldState.invalid}
                  />
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />

            <Field>
              <div className="flex items-center justify-between">
                <FieldLabel>Image env</FieldLabel>
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  onClick={() => envArray.append({ key: "", value: "" })}
                >
                  <PlusIcon /> Add env var
                </Button>
              </div>
              <FieldDescription>
                Non-secret env applied to every sandbox of this image — merged over the Dockerfile's{" "}
                <code className="font-mono">ENV</code>, under session env (ADR 0080). Applies
                immediately on save, no recapture.
              </FieldDescription>
              {envArray.fields.length === 0 ? (
                <p className="text-xs text-muted-foreground">No image env vars.</p>
              ) : (
                <div className="space-y-2">
                  {envArray.fields.map((f, i) => (
                    <ImageEnvRow
                      key={f.id}
                      control={form.control}
                      index={i}
                      onRemove={() => envArray.remove(i)}
                    />
                  ))}
                </div>
              )}
            </Field>

            <Controller
              name="workdir"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>Workdir (optional)</FieldLabel>
                  <Input
                    {...field}
                    id={field.name}
                    className="font-mono"
                    placeholder="/workspace"
                    spellCheck={false}
                    autoCapitalize="off"
                    aria-invalid={fieldState.invalid}
                  />
                  <FieldDescription>
                    Default working directory; falls back to the Dockerfile{" "}
                    <code className="font-mono">WORKDIR</code>.
                  </FieldDescription>
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />

            <div className="grid grid-cols-3 gap-4">
              <Controller
                name="vcpus"
                control={form.control}
                render={({ field, fieldState }) => (
                  <Field data-invalid={fieldState.invalid}>
                    <FieldLabel htmlFor={field.name}>vCPUs</FieldLabel>
                    <Input
                      {...field}
                      id={field.name}
                      type="number"
                      min={1}
                      step={1}
                      aria-invalid={fieldState.invalid}
                    />
                    {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                  </Field>
                )}
              />
              <Controller
                name="memoryMib"
                control={form.control}
                render={({ field, fieldState }) => (
                  <Field data-invalid={fieldState.invalid}>
                    <FieldLabel htmlFor={field.name}>Memory MiB</FieldLabel>
                    <Input
                      {...field}
                      id={field.name}
                      type="number"
                      min={1}
                      step={1}
                      placeholder="2048"
                      aria-invalid={fieldState.invalid}
                    />
                    {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                  </Field>
                )}
              />
              <Controller
                name="diskGib"
                control={form.control}
                render={({ field, fieldState }) => (
                  <Field data-invalid={fieldState.invalid}>
                    <FieldLabel htmlFor={field.name}>Disk GiB</FieldLabel>
                    <Input
                      {...field}
                      id={field.name}
                      type="number"
                      min={1}
                      step={1}
                      placeholder="16"
                      aria-invalid={fieldState.invalid}
                    />
                    {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                  </Field>
                )}
              />
              <Controller
                name="swapMib"
                control={form.control}
                render={({ field, fieldState }) => (
                  <Field data-invalid={fieldState.invalid}>
                    <FieldLabel htmlFor={field.name}>Swap MiB</FieldLabel>
                    <Input
                      {...field}
                      id={field.name}
                      type="number"
                      min={1}
                      step={1}
                      placeholder="0 (off)"
                      aria-invalid={fieldState.invalid}
                    />
                    {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                  </Field>
                )}
              />
            </div>

            {/* Warm capture hook — grouped because EVERYTHING in here is
                capture-affecting (ADR 0080: the hook runs inside the capture
                VM, so its config is baked into the base snapshot). */}
            <div className="space-y-4 rounded-md border p-4">
              <div>
                <FieldLabel>Warm capture hook</FieldLabel>
                <FieldDescription>
                  Runs inside the capture VM at base-snapshot capture. Everything in this section is
                  capture-affecting — editing it recaptures the base snapshot (minutes; sessions
                  keep working against the old snapshot until the new one is ready).
                </FieldDescription>
              </div>

              <Controller
                name="warmCommand"
                control={form.control}
                render={({ field, fieldState }) => (
                  <Field data-invalid={fieldState.invalid}>
                    <FieldLabel htmlFor={field.name}>Warm command (optional)</FieldLabel>
                    <Input
                      {...field}
                      id={field.name}
                      className="font-mono"
                      placeholder="/opt/engram/warm.sh --all"
                      spellCheck={false}
                      autoCapitalize="off"
                      aria-invalid={fieldState.invalid}
                    />
                    <FieldDescription>
                      Space-separated argv. Leave empty for no{" "}
                      <code className="font-mono">[warm]</code> hook.
                    </FieldDescription>
                    {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                  </Field>
                )}
              />

              <div className="grid grid-cols-2 gap-4">
                <Controller
                  name="warmTimeoutSecs"
                  control={form.control}
                  render={({ field, fieldState }) => (
                    <Field data-invalid={fieldState.invalid}>
                      <FieldLabel htmlFor={field.name}>Timeout secs</FieldLabel>
                      <Input
                        {...field}
                        id={field.name}
                        type="number"
                        min={1}
                        step={1}
                        placeholder="900"
                        disabled={!hasWarmCommand}
                        aria-invalid={fieldState.invalid}
                      />
                      {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                    </Field>
                  )}
                />
                <Controller
                  name="warmWorkdir"
                  control={form.control}
                  render={({ field, fieldState }) => (
                    <Field data-invalid={fieldState.invalid}>
                      <FieldLabel htmlFor={field.name}>Warm workdir</FieldLabel>
                      <Input
                        {...field}
                        id={field.name}
                        className="font-mono"
                        placeholder="/workspace"
                        spellCheck={false}
                        autoCapitalize="off"
                        disabled={!hasWarmCommand}
                        aria-invalid={fieldState.invalid}
                      />
                      {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                    </Field>
                  )}
                />
              </div>

              <Field>
                <div className="flex items-center justify-between">
                  <FieldLabel>Warm capture env</FieldLabel>
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
                  Injected into the warm command's environment at base-snapshot capture (not a
                  session secret) — ADR 0080: rides the <code className="font-mono">[warm]</code>{" "}
                  block, so it needs a warm command. Each is a literal value or an org-secret ref
                  resolved server-side at capture (names only — never values).
                </FieldDescription>
                {fields.length === 0 ? (
                  <p className="text-xs text-muted-foreground">No warm env vars.</p>
                ) : (
                  <div className="space-y-2">
                    {fields.map((f, i) => (
                      <CaptureEnvRow
                        key={f.id}
                        control={form.control}
                        index={i}
                        secretNames={orgSecretNames ?? []}
                        onRemove={() => remove(i)}
                      />
                    ))}
                  </div>
                )}
              </Field>

              {hasWarmCommand && (
                <Field>
                  <FieldLabel htmlFor="warm-net-default">Warm network</FieldLabel>
                  <Controller
                    name="netDefault"
                    control={form.control}
                    render={({ field }) => (
                      <Select value={field.value} onValueChange={field.onChange}>
                        <SelectTrigger
                          id="warm-net-default"
                          className="w-full"
                          aria-label="Warm network"
                        >
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value="none">No network (egress-less)</SelectItem>
                          <SelectItem value="deny">Deny by default, allow-list below</SelectItem>
                          <SelectItem value="allow">Allow all egress</SelectItem>
                        </SelectContent>
                      </Select>
                    )}
                  />
                  <FieldDescription>
                    Egress policy for the capture VM while the warm hook runs (ADR 0080; same shape
                    as a profile's allow-list). No network — or deny with an empty allow-list — is
                    an egress-less capture.
                  </FieldDescription>
                  {netDefaultLive === "deny" && (
                    <div className="grid gap-3 sm:grid-cols-2">
                      <Controller
                        name="allowHostsText"
                        control={form.control}
                        render={({ field }) => (
                          <Field>
                            <FieldLabel htmlFor="warm-allow-hosts">Allowed hosts</FieldLabel>
                            <Textarea
                              {...field}
                              id="warm-allow-hosts"
                              rows={3}
                              placeholder={"registry.npmjs.org\nproxy.golang.org"}
                              className="font-mono text-sm"
                            />
                          </Field>
                        )}
                      />
                      <Controller
                        name="allowPatternsText"
                        control={form.control}
                        render={({ field }) => (
                          <Field>
                            <FieldLabel htmlFor="warm-allow-patterns">Host patterns</FieldLabel>
                            <Textarea
                              {...field}
                              id="warm-allow-patterns"
                              rows={3}
                              placeholder={"*.githubusercontent.com\n*.pypi.org"}
                              className="font-mono text-sm"
                            />
                          </Field>
                        )}
                      />
                    </div>
                  )}
                </Field>
              )}
            </div>

            {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
          </FieldGroup>

          {/* ADR 0080: a capture-affecting edit was refused with
              FailedPrecondition — confirm before re-sending with
              allow_recapture=true. */}
          {pendingRecapture && (
            <div className="mt-4 space-y-2 rounded-md border border-destructive/40 bg-destructive/5 p-3">
              <p className="text-sm font-medium text-destructive">
                This edit changes capture-affecting fields
              </p>
              <p className="font-mono text-xs text-muted-foreground">{pendingRecapture.reason}</p>
              <p className="text-xs text-muted-foreground">
                Applying it will recapture the base snapshot — that takes minutes, and sessions keep
                working against the old snapshot until the new one is ready. Progress shows in the
                jobs list above.
              </p>
              <div className="flex justify-end gap-2">
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  onClick={() => setPendingRecapture(null)}
                  disabled={update.isPending}
                >
                  Keep editing
                </Button>
                <Button
                  type="button"
                  variant="destructive"
                  size="sm"
                  onClick={onConfirmRecapture}
                  disabled={update.isPending}
                >
                  {update.isPending ? "Recapturing…" : "Recapture and apply"}
                </Button>
              </div>
            </div>
          )}

          {!pendingRecapture && (
            <DialogFooter className="mt-4 sm:items-center">
              <Button
                type="button"
                variant="ghost"
                onClick={() => onOpenChange(false)}
                disabled={busy}
              >
                Cancel
              </Button>
              <Button type="submit" disabled={busy}>
                {busy ? (isEdit ? "Saving…" : "Enabling…") : isEdit ? "Save" : "Enable"}
              </Button>
            </DialogFooter>
          )}
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

// ADR 0036 amendment (issue #538): `prestage_hosts` is a JSON-encoded
// `{"<host-uuid>": {"outcome": "staged"|"timed_out"|"unschedulable",
// "waited_ms"?: number}}` map, written once at prestage-stage end (stays
// on the row through `ready`/`failed` as the audit record). "{}" before
// the stage has run. Malformed/absent JSON renders nothing rather than
// throwing — this is a best-effort operator surface, not load-bearing.
function summarizePrestageHosts(raw: string): string | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) return null;
  let staged = 0;
  let timedOut = 0;
  let unschedulable = 0;
  for (const entry of Object.values(parsed as Record<string, { outcome?: string }>)) {
    switch (entry?.outcome) {
      case "staged":
        staged++;
        break;
      case "timed_out":
        timedOut++;
        break;
      case "unschedulable":
        unschedulable++;
        break;
    }
  }
  const eligible = staged + timedOut;
  if (eligible === 0) return null;
  const suffix = unschedulable > 0 ? ` (${unschedulable} unschedulable)` : "";
  return `${staged}/${eligible} hosts staged${suffix}`;
}

/// Parse a JSON-encoded stage-record array (`materialize_stages` /
/// `warm_stages`). Malformed/absent JSON renders nothing rather than
/// throwing — best-effort operator surface.
function parseStages(raw: string): StageRecord[] {
  try {
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? (parsed as StageRecord[]) : [];
  } catch {
    return [];
  }
}

function fmtDuration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m${s % 60 ? ` ${s % 60}s` : ""}`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}

function stageDurationMs(s: StageRecord): number | null {
  const start = new Date(s.started_at).getTime();
  if (Number.isNaN(start)) return null;
  const end = s.ended_at ? new Date(s.ended_at).getTime() : Date.now();
  return Math.max(0, end - start);
}

/// One enable timeline (materialize or warm) as inline stage chips:
/// `pull 2m46s ✓ → flatten 12m ⏱ (typically ~48m)`. The open stage
/// ticks on every poll re-render (the jobs query polls at 2 s while any
/// job is active). `reference` supplies the previous successful run's
/// per-stage durations — the "typically ~X" ETA denominators.
function StageTimeline({
  label,
  stages,
  reference,
  jobFailed,
}: {
  label: string;
  stages: StageRecord[];
  reference: StageRecord[];
  jobFailed: boolean;
}) {
  if (stages.length === 0) return null;
  const typicalMs = new Map<string, number>();
  for (const r of reference) {
    const d = r.ended_at ? stageDurationMs(r) : null;
    if (d !== null) typicalMs.set(r.name, d);
  }
  return (
    <div className="flex flex-wrap items-center gap-x-1 gap-y-0.5 text-xs text-muted-foreground">
      <span className="text-[0.7rem] font-medium">{label}</span>
      {stages.map((s, i) => {
        const open = s.ended_at === null;
        const d = stageDurationMs(s);
        const typical = open ? typicalMs.get(s.name) : undefined;
        return (
          <span key={`${s.name}-${i}`} className="flex items-center gap-1">
            {i > 0 && <span className="text-muted-foreground/50">→</span>}
            <span className={open && !jobFailed ? "font-medium text-foreground" : undefined}>
              {s.name}
            </span>
            {d !== null && <span className="font-mono">{fmtDuration(d)}</span>}
            {open ? (
              jobFailed ? (
                <span className="text-destructive" title="the attempt died in this stage">
                  ✗
                </span>
              ) : (
                <span className="animate-pulse">⏱</span>
              )
            ) : (
              <span className="text-muted-foreground/70">✓</span>
            )}
            {typical !== undefined && !jobFailed && (
              <span className="text-muted-foreground/60">(typically ~{fmtDuration(typical)})</span>
            )}
          </span>
        );
      })}
    </div>
  );
}

function EnableJobRow({ job, reference }: { job: EnableJob; reference?: EnableJob }) {
  const retry = useRetryEnableJob();
  const failed = job.state === "failed";
  const active = isJobActive(job);
  const capturing = job.state === "capturing";
  const materializing = job.state === "materializing";
  const pct =
    job.chunks_total && job.chunks_total > 0
      ? Math.min(100, Math.round((job.chunks_done / job.chunks_total) * 100))
      : null;
  const prestageSummary = summarizePrestageHosts(job.prestage_hosts);

  const materializeStages = parseStages(job.materialize_stages);
  const warmStages = parseStages(job.warm_stages);
  const refMaterialize = reference ? parseStages(reference.materialize_stages) : [];
  const refWarm = reference ? parseStages(reference.warm_stages) : [];

  // The live substage, most-specific first: an open materialize stage
  // while materializing, else issue #539's capture_phase/warm_stage.
  const openMaterialize = materializing ? materializeStages.find((s) => s.ended_at === null) : null;
  const captureDetail =
    capturing && (job.capture_phase || job.warm_stage)
      ? [job.capture_phase, job.warm_stage].filter(Boolean).join(" · ")
      : null;

  // Wall-clock context: total job age, ticked by the 2 s active poll.
  const totalMs = Date.now() - new Date(job.created_at).getTime();

  return (
    <li>
      <Card className={failed ? "border-destructive/40" : undefined}>
        <CardContent className="space-y-2 py-3">
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-mono text-sm whitespace-nowrap">{job.image_uri}</span>
            <Badge variant={failed ? "destructive" : "secondary"} className="font-normal">
              {JOB_STATE_LABEL[job.state]}
              {openMaterialize ? ` · ${openMaterialize.name}` : ""}
              {materializing && job.chunks_total
                ? ` · ${job.chunks_done}/${job.chunks_total} chunks`
                : ""}
            </Badge>
            {captureDetail && (
              <Badge variant="outline" className="font-mono font-normal">
                {captureDetail}
              </Badge>
            )}
            {prestageSummary && (
              <Badge variant="outline" className="font-normal">
                {prestageSummary}
              </Badge>
            )}
            {job.attempts > 1 && (
              <Badge variant="outline" className="font-normal" title="pipeline attempts so far">
                attempt {job.attempts}
              </Badge>
            )}
            {job.materialize_host_id && (
              <Badge
                variant="outline"
                className="font-mono font-normal"
                title={`materialize host ${job.materialize_host_id}`}
              >
                host {job.materialize_host_id.slice(0, 8)}
              </Badge>
            )}
            {active && !Number.isNaN(totalMs) && (
              <span className="text-xs text-muted-foreground font-mono">
                {fmtDuration(totalMs)}
              </span>
            )}
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
          <StageTimeline
            label="materialize"
            stages={materializeStages}
            reference={refMaterialize}
            jobFailed={failed}
          />
          <StageTimeline label="warm" stages={warmStages} reference={refWarm} jobFailed={failed} />
          {pct !== null && !failed && <Progress value={pct} className="h-1 max-w-md" />}
          {failed && job.error && <p className="text-xs text-destructive">{job.error}</p>}
          {failed && job.warm_stage && (
            <p className="text-xs text-muted-foreground">
              failed at [warm] stage <span className="font-mono">{job.warm_stage}</span>
            </p>
          )}
          {/* The live tail is the "something is happening" signal a
              45-minute flatten or 10-minute JVM boot needs: during
              materialize it's the current stage frame; during capture,
              the [warm] hook's rolling last 16 KiB (the per-service
              "brain-backend ready in 527s" lines). Collapsed by default
              while running; the failure rendering keeps it expanded. */}
          {!failed && job.output_tail && (
            <details className="text-xs">
              <summary className="cursor-pointer text-muted-foreground select-none">
                live output
              </summary>
              <pre className="mt-1 max-h-40 overflow-y-auto rounded bg-muted p-2 whitespace-pre-wrap">
                {job.output_tail}
              </pre>
            </details>
          )}
          {failed && job.output_tail && (
            <pre className="max-h-40 overflow-y-auto rounded bg-muted p-2 text-xs whitespace-pre-wrap">
              {job.output_tail}
            </pre>
          )}
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
