import { useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import { useOrgSecrets, usePutOrgSecret, useDeleteOrgSecret } from "../../hooks/useOrgSecrets";
import type { OrgSecretMeta } from "../../gen/engram/app/v1/org_secret_pb";
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

// Org secrets (ADR 0057 C0) — the admin-managed, KEK-sealed value store that
// profile secrets, integration inject credentials, and the GitHub App mint key
// all resolve through (the composed SecretStore's PG layer). Values are sealed
// at the coordinator on write and NEVER returned: the panel only ever shows the
// name + key id + timestamps ("set / not set"). The name is the `ref` a profile
// secret points at.
export function SecretsPanel() {
  const { data, isLoading, error } = useOrgSecrets();
  const rows = data?.secrets ?? [];

  return (
    <div className="space-y-6">
      <PageHeading
        title="Org secrets"
        description="The admin-managed secret store. Values are sealed under the deployment key the moment you save them, never returned to the browser. A profile or connector references a secret by its name."
        actions={<SecretDialog />}
      />

      {error && (
        <p className="text-sm text-destructive">could not load secrets — {String(error)}</p>
      )}

      {isLoading ? (
        <p className="py-6 text-sm text-muted-foreground">Loading…</p>
      ) : rows.length === 0 ? (
        <Card>
          <CardContent className="py-10 text-center text-sm text-muted-foreground">
            No org secrets yet. Add one, then reference it by name from a profile's secrets or an
            integration's credential.
          </CardContent>
        </Card>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Key id</TableHead>
              <TableHead>Updated</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((row) => (
              <SecretRow key={row.name} row={row} />
            ))}
          </TableBody>
        </Table>
      )}
    </div>
  );
}

function SecretRow({ row }: { row: OrgSecretMeta }) {
  const del = useDeleteOrgSecret();
  return (
    <TableRow>
      <TableCell className="font-mono text-sm">{row.name}</TableCell>
      <TableCell className="font-mono text-xs text-muted-foreground">
        <Badge variant="outline">{row.keyId || "—"}</Badge>
      </TableCell>
      <TableCell
        className="font-mono text-xs text-muted-foreground"
        title={row.updatedAt ? new Date(row.updatedAt).toLocaleString() : undefined}
      >
        {row.updatedAt ? timeAgo(row.updatedAt) : "—"}
      </TableCell>
      <TableCell>
        <div className="flex items-center justify-end gap-1">
          <SecretDialog existingName={row.name} />
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="ghost" size="sm" disabled={del.isPending}>
                Remove
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Remove {row.name}?</AlertDialogTitle>
                <AlertDialogDescription>
                  Any profile or connector that references this name will stop resolving it on the
                  next session create. The sealed value is deleted.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction onClick={() => del.mutate({ name: row.name })}>
                  Remove secret
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {del.error && (
          <p className="mt-1 text-right text-xs text-destructive">
            could not remove — {String(del.error)}
          </p>
        )}
      </TableCell>
    </TableRow>
  );
}

// Org-secret names are the `ref` profiles/connectors point at. Keep them to a
// conservative identifier set so a typo can't silently shadow a deployment-backed
// (e.g. gcp-sm://) ref; the coordinator is authoritative.
const NAME_RE = /^[A-Za-z0-9._:/-]+$/;
const secretSchema = z.object({
  name: z
    .string()
    .trim()
    .min(1, "name is required")
    .regex(NAME_RE, "letters, digits, and . _ : / - only"),
  value: z.string().min(1, "value is required"),
});
type SecretValues = z.infer<typeof secretSchema>;

/** Add (name editable) or Replace (name fixed) — both upsert via PutSecret. */
function SecretDialog({ existingName }: { existingName?: string }) {
  const [open, setOpen] = useState(false);
  const put = usePutOrgSecret();
  const isReplace = existingName !== undefined;
  const form = useForm<SecretValues>({
    resolver: zodResolver(secretSchema),
    defaultValues: { name: existingName ?? "", value: "" },
  });

  const onSubmit = async (data: SecretValues) => {
    try {
      await put.mutateAsync({ name: data.name.trim(), value: data.value });
      form.reset({ name: existingName ?? "", value: "" });
      setOpen(false);
    } catch (e) {
      form.setError("root", { message: String(e) });
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (o) form.reset({ name: existingName ?? "", value: "" });
      }}
    >
      <DialogTrigger asChild>
        {isReplace ? (
          <Button variant="ghost" size="sm">
            Replace
          </Button>
        ) : (
          <Button>Add secret</Button>
        )}
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{isReplace ? `Replace ${existingName}` : "New org secret"}</DialogTitle>
          <DialogDescription>
            The value is sealed under the deployment KEK before it touches Postgres and is never
            returned by the API.
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={form.handleSubmit(onSubmit)}>
          <FieldGroup>
            <Controller
              name="name"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>Name</FieldLabel>
                  <Input
                    {...field}
                    id={field.name}
                    autoFocus={!isReplace}
                    readOnly={isReplace}
                    className="font-mono"
                    placeholder="sentry-token"
                    spellCheck={false}
                    autoCapitalize="off"
                    aria-invalid={fieldState.invalid}
                  />
                  <FieldDescription>
                    The reference name a profile secret or connector credential points at.
                  </FieldDescription>
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />
            <Controller
              name="value"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>Value</FieldLabel>
                  <Input
                    {...field}
                    id={field.name}
                    type="password"
                    autoFocus={isReplace}
                    className="font-mono"
                    autoComplete="new-password"
                    placeholder="•••••"
                    aria-invalid={fieldState.invalid}
                  />
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
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
              disabled={put.isPending}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={put.isPending}>
              {put.isPending ? "Sealing & saving…" : "Save"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
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
