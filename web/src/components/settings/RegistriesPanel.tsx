import { useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import { useAddRegistry, useDeleteRegistry, useRegistries } from "../../hooks/useRegistries";
import type { RegistryAuthKind, RegistryCredentialSummary } from "../../lib/types";
import { PageHeading } from "../page-heading";
import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { Badge } from "@/components/ui/badge";
import { textVariants } from "@/components/ui/text";
import { cn } from "@/lib/utils";
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
  Field,
  FieldContent,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSet,
  FieldTitle,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
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
import { relativeAge } from "@/lib/relative-time";

// Registries panel — operators register Docker registries with a static
// credential (sealed under the deployment KEK) or an ambient cloud-IAM
// identity: GCP Workload Identity or AWS ECR (ADR 0122). The kind matrix
// mirrors the RegistryAuthSpec enum.
export function RegistriesPanel() {
  const { data, isLoading, error } = useRegistries();
  const rows = data ?? [];

  return (
    <div className="space-y-6">
      <PageHeading
        title="Registries"
        count={rows.length || undefined}
        actions={<AddRegistryDialog />}
      />

      {error && <EmptyState tone="error">Could not load registries — {String(error)}</EmptyState>}

      {isLoading ? (
        <SkeletonRows
          rows={3}
          columns={["minmax(12rem,1fr)", "8rem", "minmax(10rem,1fr)", "6rem", "6rem"]}
        />
      ) : rows.length === 0 ? (
        <EmptyState>
          No registries yet. Register a Docker registry to enable image pulls from outside your
          local network.
        </EmptyState>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Host</TableHead>
              <TableHead>Auth</TableHead>
              <TableHead>Principal</TableHead>
              <TableHead>Added</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((row) => (
              <RegistryRow key={row.id} row={row} />
            ))}
          </TableBody>
        </Table>
      )}
    </div>
  );
}

function RegistryRow({ row }: { row: RegistryCredentialSummary }) {
  const del = useDeleteRegistry();
  return (
    <TableRow>
      <TableCell className="font-mono text-sm">{row.registry_host}</TableCell>
      <TableCell>
        <Badge variant="outline">
          {row.auth_kind === "static"
            ? "static"
            : row.auth_kind === "aws_ecr"
              ? "aws ecr"
              : row.auth_kind === "anonymous"
                ? "anonymous"
                : "workload identity"}
        </Badge>
      </TableCell>
      <TableCell className="font-mono text-xs text-muted-foreground">
        {row.auth_principal || "—"}
      </TableCell>
      <TableCell
        className="font-mono text-xs text-muted-foreground"
        title={new Date(row.created_at).toLocaleString()}
      >
        {relativeAge(row.created_at)}
      </TableCell>
      <TableCell>
        <div className="flex items-center justify-end">
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="ghost" size="sm" disabled={del.isPending}>
                Remove
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Remove {row.registry_host}?</AlertDialogTitle>
                <AlertDialogDescription>
                  Sessions can no longer pull images from this registry. The sealed credential is
                  deleted.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction onClick={() => del.mutate({ host: row.registry_host })}>
                  Remove registry
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {del.error && (
          <EmptyState tone="error" inline className="mt-1 items-end text-right">
            Could not remove — {String(del.error)}
          </EmptyState>
        )}
      </TableCell>
    </TableRow>
  );
}

interface AuthKindCardSpec {
  kind: RegistryAuthKind;
  label: string;
  blurb: string;
  disabled?: boolean;
  hint?: string;
}

const KIND_CARDS: AuthKindCardSpec[] = [
  {
    kind: "static",
    label: "Static",
    blurb:
      "Username + password sealed under the deployment KEK. DockerHub, GHCR, Quay, Harbor, GAR with a service-account JSON key.",
  },
  {
    kind: "gcp_workload_identity",
    label: "GCP Workload Identity",
    blurb:
      "Ambient GCP identity exchanged for a short-lived token per pull. No stored secret material.",
  },
  {
    kind: "aws_ecr",
    label: "AWS ECR",
    blurb:
      "The coordinator's ambient AWS IAM identity (IRSA / instance role) is exchanged for an ECR token per pull. No stored secret material.",
  },
];

const registrySchema = z
  .object({
    host: z.string().trim().min(1, "host is required"),
    authKind: z.enum(["static", "gcp_workload_identity", "aws_ecr"]),
    username: z.string(),
    password: z.string(),
    impersonateSa: z.string(),
    assumeRoleArn: z.string(),
  })
  .superRefine((val, ctx) => {
    if (val.authKind === "static") {
      if (!val.username.trim()) {
        ctx.addIssue({ code: "custom", path: ["username"], message: "username is required" });
      }
      if (!val.password) {
        ctx.addIssue({ code: "custom", path: ["password"], message: "password is required" });
      }
    }
    if (
      val.authKind === "aws_ecr" &&
      !/\.dkr\.ecr\.[a-z0-9-]+\.amazonaws\.com(\.cn)?$/.test(val.host.trim())
    ) {
      ctx.addIssue({
        code: "custom",
        path: ["host"],
        message: "AWS ECR auth needs an ECR host (<account>.dkr.ecr.<region>.amazonaws.com)",
      });
    }
  });
type RegistryValues = z.infer<typeof registrySchema>;

function AddRegistryDialog() {
  const [open, setOpen] = useState(false);
  const add = useAddRegistry();
  const form = useForm<RegistryValues>({
    resolver: zodResolver(registrySchema),
    defaultValues: {
      host: "",
      authKind: "static",
      username: "",
      password: "",
      impersonateSa: "",
      assumeRoleArn: "",
    },
  });
  const authKind = form.watch("authKind");

  const onSubmit = async (data: RegistryValues) => {
    // Build the proto oneof auth field. MessageInitShape allows plain objects
    // for message fields; we assert the type here to satisfy the compiler.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    let auth: any;
    if (data.authKind === "static") {
      auth = {
        case: "static",
        value: { username: data.username.trim(), password: data.password },
      };
    } else if (data.authKind === "aws_ecr") {
      // No assumeRoleArn: the STS chain is not implemented at pull
      // time and the API rejects a value eagerly — the field below is
      // disabled until it lands.
      auth = { case: "awsEcr", value: {} };
    } else {
      auth = {
        case: "gcpWorkloadIdentity",
        value: { impersonateSa: data.impersonateSa.trim() || undefined },
      };
    }
    try {
      await add.mutateAsync({ host: data.host.trim(), auth });
      form.reset();
      setOpen(false);
    } catch (e) {
      form.setError("root", { message: String(e) });
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button>Register a new registry</Button>
      </DialogTrigger>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>New registry</DialogTitle>
          <DialogDescription>
            Passwords are sealed under the deployment KEK before they touch Postgres; never returned
            by the API.
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={form.handleSubmit(onSubmit)}>
          <FieldGroup>
            <Controller
              name="host"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>Host</FieldLabel>
                  <Input
                    {...field}
                    id={field.name}
                    autoFocus
                    className="font-mono"
                    placeholder="ghcr.io"
                    spellCheck={false}
                    autoCapitalize="off"
                    aria-invalid={fieldState.invalid}
                  />
                  <FieldDescription>
                    e.g. ghcr.io · gcr.io · us-east1-docker.pkg.dev · localhost:5001
                  </FieldDescription>
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />

            <Controller
              name="authKind"
              control={form.control}
              render={({ field }) => (
                <FieldSet>
                  <FieldLegend variant="label">Auth model</FieldLegend>
                  <RadioGroup
                    value={field.value}
                    onValueChange={field.onChange}
                    aria-label="Authentication model"
                    className="gap-2"
                  >
                    {KIND_CARDS.map((card) => (
                      <FieldLabel key={card.kind} htmlFor={`reg-auth-${card.kind}`}>
                        <Field orientation="horizontal" data-disabled={card.disabled || undefined}>
                          <FieldContent>
                            <FieldTitle>
                              {card.label}
                              {card.hint && (
                                <Badge
                                  variant="secondary"
                                  className={cn(textVariants({ variant: "label" }), "text-2xs")}
                                >
                                  {card.hint}
                                </Badge>
                              )}
                            </FieldTitle>
                            <FieldDescription>{card.blurb}</FieldDescription>
                          </FieldContent>
                          <RadioGroupItem
                            value={card.kind}
                            id={`reg-auth-${card.kind}`}
                            disabled={card.disabled}
                          />
                        </Field>
                      </FieldLabel>
                    ))}
                  </RadioGroup>
                </FieldSet>
              )}
            />

            {authKind === "static" ? (
              <>
                <Controller
                  name="username"
                  control={form.control}
                  render={({ field, fieldState }) => (
                    <Field data-invalid={fieldState.invalid}>
                      <FieldLabel htmlFor={field.name}>Username</FieldLabel>
                      <Input
                        {...field}
                        id={field.name}
                        className="font-mono"
                        spellCheck={false}
                        autoCapitalize="off"
                        autoComplete="username"
                        placeholder="username or _json_key"
                        aria-invalid={fieldState.invalid}
                      />
                      <FieldDescription>
                        For GCP service-account JSON keys, the literal string{" "}
                        <code className="font-mono">_json_key</code>.
                      </FieldDescription>
                      {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                    </Field>
                  )}
                />
                <Controller
                  name="password"
                  control={form.control}
                  render={({ field, fieldState }) => (
                    <Field data-invalid={fieldState.invalid}>
                      <FieldLabel htmlFor={field.name}>Password</FieldLabel>
                      <Input
                        {...field}
                        id={field.name}
                        type="password"
                        className="font-mono"
                        autoComplete="new-password"
                        placeholder="•••••"
                        aria-invalid={fieldState.invalid}
                      />
                      {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                    </Field>
                  )}
                />
              </>
            ) : authKind === "aws_ecr" ? (
              <>
                <FieldDescription>
                  No password required. The coordinator's ambient AWS IAM identity (IRSA or the node
                  instance role) is exchanged for an ECR token on every pull — grant{" "}
                  <code className="font-mono">ecr:GetAuthorizationToken</code> to the coordinator's
                  role.
                </FieldDescription>
                <Field>
                  <FieldLabel htmlFor="assumeRoleArn">Assume role</FieldLabel>
                  <Input
                    id="assumeRoleArn"
                    className="font-mono"
                    placeholder="arn:aws:iam::123456789012:role/engram-pull"
                    disabled
                  />
                  <FieldDescription>
                    Cross-account pulls via STS AssumeRole — coming soon; the ambient identity is
                    used until then.
                  </FieldDescription>
                </Field>
              </>
            ) : (
              <>
                <FieldDescription>
                  No password required. The host-agent's ambient GCP identity is exchanged for a
                  short-lived OAuth token on every pull.
                </FieldDescription>
                <Controller
                  name="impersonateSa"
                  control={form.control}
                  render={({ field }) => (
                    <Field>
                      <FieldLabel htmlFor={field.name}>Impersonate (optional)</FieldLabel>
                      <Input
                        {...field}
                        id={field.name}
                        className="font-mono"
                        placeholder="engram@my-project.iam.gserviceaccount.com"
                        spellCheck={false}
                        autoCapitalize="off"
                      />
                      <FieldDescription>
                        Pull as a different service account via the IAM Credentials API. Leave empty
                        to use the ambient identity.
                      </FieldDescription>
                    </Field>
                  )}
                />
              </>
            )}

            {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
          </FieldGroup>

          <DialogFooter className="mt-5">
            <Button
              type="button"
              variant="ghost"
              onClick={() => setOpen(false)}
              disabled={add.isPending}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={add.isPending}>
              {add.isPending ? "Sealing & saving…" : "Register"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
