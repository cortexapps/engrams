import { useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import {
  useHarnessCatalog,
  useRegisterHarness,
  useDeleteHarness,
} from "../../hooks/useHarnessCatalog";
import { useOrgSecrets, usePutOrgSecret } from "../../hooks/useOrgSecrets";
import type { HarnessSummary } from "../../gen/engram/app/v1/harness_pb";
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

// Harnesses (ADR 0062/0063) — the admin view of the agent harnesses sessions can
// run. The built-in `claude` rides the fleet stamp; custom harnesses are
// registered by OCI ref. Each declares the model/effort options profiles pick
// from and the credential env-var names it needs: `org_env` (the programmatic
// API key — an admin sets it here as an org secret) and `user_env` (the
// per-user token, set by each member under My Tokens).
export function HarnessesPanel() {
  const { data, isLoading, error } = useHarnessCatalog();
  const harnesses = data ?? [];

  return (
    <div className="space-y-6">
      <PageHeading
        title="Harnesses"
        count={harnesses.length || undefined}
        actions={<RegisterDialog />}
      />

      {error && (
        <p className="text-sm text-destructive">Could not load harnesses — {String(error)}</p>
      )}

      {isLoading ? (
        <p className="py-6 text-sm text-muted-foreground">Loading…</p>
      ) : harnesses.length === 0 ? (
        <Card>
          <CardContent className="py-10 text-center text-sm text-muted-foreground">
            No harnesses registered. Register one by OCI ref to make it selectable on profiles.
          </CardContent>
        </Card>
      ) : (
        <div className="space-y-4">
          {harnesses.map((h) => (
            <HarnessCard key={h.name} harness={h} />
          ))}
        </div>
      )}
    </div>
  );
}

function HarnessCard({ harness }: { harness: HarnessSummary }) {
  const d = harness.descriptor;
  const del = useDeleteHarness();
  const orgEnv = d?.auth?.orgEnv;
  const userEnv = d?.auth?.userEnv;

  return (
    <Card>
      <CardContent className="space-y-3 py-5">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <span className="font-medium">{d?.label || harness.name}</span>
              <Badge variant="outline" className="font-mono text-xs">
                {harness.name}
              </Badge>
              {harness.builtIn && <Badge variant="secondary">built-in</Badge>}
            </div>
            {d?.description && (
              <p className="mt-1 text-sm text-muted-foreground">{d.description}</p>
            )}
          </div>
          {!harness.builtIn && (
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button variant="ghost" size="sm" disabled={del.isPending}>
                  Remove
                </Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Remove {d?.label || harness.name}?</AlertDialogTitle>
                  <AlertDialogDescription>
                    Profiles that select this harness will fail to create a session until they pick
                    another. The staged bundle is GC'd once no snapshot pins it.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction onClick={() => del.mutate({ name: harness.name })}>
                    Remove harness
                  </AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          )}
        </div>

        {d && (d.models.length > 0 || d.effort.length > 0) && (
          <div className="flex flex-wrap gap-x-6 gap-y-1 text-xs text-muted-foreground">
            {d.models.length > 0 && (
              <span>
                <span className="font-medium">Models:</span>{" "}
                {d.models.map((m) => m.label || m.id).join(", ")}
              </span>
            )}
            {d.effort.length > 0 && (
              <span>
                <span className="font-medium">Effort:</span>{" "}
                {d.effort.map((e) => e.label || e.id).join(", ")}
              </span>
            )}
          </div>
        )}

        {orgEnv && <OrgCredentialRow envVar={orgEnv} />}
        {userEnv && (
          <p className="text-xs text-muted-foreground">
            <span className="font-medium">User credential:</span>{" "}
            <span className="font-mono">{userEnv}</span> — set per-user under{" "}
            <span className="font-medium">My Tokens</span> (human/interactive runs).
          </p>
        )}

        {del.error && (
          <p className="text-xs text-destructive">Could not remove — {String(del.error)}</p>
        )}
      </CardContent>
    </Card>
  );
}

/** The harness's programmatic credential — an org secret named `envVar`. Shows
 *  whether it's configured + a set/replace dialog (reuses the org-secret store). */
function OrgCredentialRow({ envVar }: { envVar: string }) {
  const { data } = useOrgSecrets();
  const isSet = (data?.secrets ?? []).some((s) => s.name === envVar);
  return (
    <div className="flex items-center justify-between gap-3 rounded-md border border-border/60 px-3 py-2">
      <div className="min-w-0 text-xs">
        <div>
          <span className="font-medium">Org credential:</span>{" "}
          <span className="font-mono">{envVar}</span>{" "}
          {isSet ? (
            <Badge variant="secondary">configured</Badge>
          ) : (
            <Badge variant="destructive">not set</Badge>
          )}
        </div>
        <p className="mt-0.5 text-muted-foreground">
          The API key injected on programmatic (Slack/cron/API) runs. Sealed under the deployment
          key; never returned.
        </p>
      </div>
      <SetSecretDialog envVar={envVar} isSet={isSet} />
    </div>
  );
}

const secretSchema = z.object({ value: z.string().min(1, "value is required") });
type SecretValues = z.infer<typeof secretSchema>;

function SetSecretDialog({ envVar, isSet }: { envVar: string; isSet: boolean }) {
  const [open, setOpen] = useState(false);
  const put = usePutOrgSecret();
  const form = useForm<SecretValues>({
    resolver: zodResolver(secretSchema),
    defaultValues: { value: "" },
  });

  const onSubmit = async (vals: SecretValues) => {
    try {
      await put.mutateAsync({ name: envVar, value: vals.value });
      form.reset({ value: "" });
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
        if (o) form.reset({ value: "" });
      }}
    >
      <DialogTrigger asChild>
        <Button variant={isSet ? "ghost" : "default"} size="sm">
          {isSet ? "Replace" : "Set"}
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>
            {isSet ? "Replace" : "Set"} <span className="font-mono">{envVar}</span>
          </DialogTitle>
          <DialogDescription>
            Stored as the org secret <span className="font-mono">{envVar}</span> — sealed under the
            deployment key before it touches Postgres and never returned by the API.
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={form.handleSubmit(onSubmit)}>
          <FieldGroup>
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
                    autoFocus
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

const NAME_RE = /^[a-z0-9][a-z0-9_-]*$/;
const registerSchema = z.object({
  name: z
    .string()
    .trim()
    .min(1, "name is required")
    .regex(NAME_RE, "lowercase letters, digits, _ and - (a stable id)"),
  ociRef: z.string().trim().min(1, "OCI ref is required"),
});
type RegisterValues = z.infer<typeof registerSchema>;

/** Register a custom harness by OCI ref (the coordinator pulls + packs it). */
function RegisterDialog() {
  const [open, setOpen] = useState(false);
  const register = useRegisterHarness();
  const form = useForm<RegisterValues>({
    resolver: zodResolver(registerSchema),
    defaultValues: { name: "", ociRef: "" },
  });

  const onSubmit = async (vals: RegisterValues) => {
    try {
      await register.mutateAsync({ name: vals.name.trim(), ociRef: vals.ociRef.trim(), owner: "" });
      form.reset({ name: "", ociRef: "" });
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
        if (o) form.reset({ name: "", ociRef: "" });
      }}
    >
      <DialogTrigger asChild>
        <Button>Register harness</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Register a harness</DialogTitle>
          <DialogDescription>
            Point at an OCI artifact carrying the harness tree (entry binary + its `harness.toml`).
            The coordinator pulls it, validates the descriptor, and packs the bundle. The name can't
            shadow a built-in.
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
                    autoFocus
                    className="font-mono"
                    placeholder="opencode"
                    spellCheck={false}
                    autoCapitalize="off"
                    aria-invalid={fieldState.invalid}
                  />
                  <FieldDescription>
                    The stable id profiles select (== `CreateSessionRequest.harness`).
                  </FieldDescription>
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />
            <Controller
              name="ociRef"
              control={form.control}
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor={field.name}>OCI ref</FieldLabel>
                  <Input
                    {...field}
                    id={field.name}
                    className="font-mono"
                    placeholder="ghcr.io/acme/opencode-harness:v1"
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
          <DialogFooter className="mt-5">
            <Button
              type="button"
              variant="ghost"
              onClick={() => setOpen(false)}
              disabled={register.isPending}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={register.isPending}>
              {register.isPending ? "Registering…" : "Register"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
