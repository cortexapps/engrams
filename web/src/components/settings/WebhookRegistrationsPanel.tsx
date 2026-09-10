/** Custom (generic HMAC) webhook registrations — the escape hatch for
 * providers without a connector. Moved verbatim from the legacy Automations
 * page (ADR 0119 phase 3.2); behavior unchanged: generic-only schemes, the
 * one-time secret reveal, and the retired-registration banner (#1334).
 */
import { useMemo, useState } from "react";
import { toast } from "sonner";
import { CheckIcon, CopyIcon, PlusIcon, Trash2Icon, WebhookIcon } from "lucide-react";

import type { WebhookRegistration } from "@/gen/engram/app/v1/automation_pb";
import { useConnectors } from "@/hooks/useIntegrations";
import {
  useCreateWebhookRegistration,
  useDeleteWebhookRegistration,
  useWebhookRegistrations,
} from "@/hooks/useAutomations";
import { webhookConnectorHints, type VerificationScheme } from "@/lib/automations";
import { errorMessage } from "@/lib/errors";
import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Field, FieldDescription, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

const SCHEME_LABELS: Record<VerificationScheme, string> = {
  generic_hmac_sha256: "Generic HMAC SHA-256",
};

interface CreatedRegistration {
  id: string;
  secret: string;
}

function CreateRegistrationDialog() {
  const connectors = useConnectors();
  const createRegistration = useCreateWebhookRegistration();
  const hints = useMemo(
    () => webhookConnectorHints(connectors.data?.connectors ?? []),
    [connectors.data?.connectors],
  );
  const [open, setOpen] = useState(false);
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [scheme, setScheme] = useState<VerificationScheme>("generic_hmac_sha256");
  const [providerHint, setProviderHint] = useState("");
  const [created, setCreated] = useState<CreatedRegistration | null>(null);
  const [error, setError] = useState("");
  const [copied, setCopied] = useState<"url" | "secret" | null>(null);

  const reset = () => {
    setId("");
    setName("");
    setScheme("generic_hmac_sha256");
    setProviderHint("");
    setCreated(null);
    setError("");
    setCopied(null);
  };

  const onOpenChange = (next: boolean) => {
    setOpen(next);
    if (!next) reset();
  };

  const onProviderChange = (provider: string) => {
    const value = provider === "generic" ? "" : provider;
    setProviderHint(value);
  };

  const onCreate = async () => {
    setError("");
    try {
      const response = await createRegistration.mutateAsync({
        id,
        name,
        verificationScheme: scheme,
        ...(providerHint ? { providerHint } : {}),
      });
      if (!response.registration) throw new Error("Registration was not returned");
      setCreated({ id: response.registration.id, secret: response.secret });
    } catch (createError) {
      setError(errorMessage(createError));
    }
  };

  const copy = async (kind: "url" | "secret", value: string) => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(kind);
      toast.success(kind === "url" ? "Hook URL copied" : "Secret copied");
    } catch {
      toast.error("Could not copy to clipboard");
    }
  };

  const hookPath = created ? `/api/v1/hooks/${created.id}` : "";

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm">
          <PlusIcon className="size-3.5" />
          New webhook
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-xl" showCloseButton={!created}>
        {created ? (
          <>
            <DialogHeader>
              <DialogTitle>Webhook ready</DialogTitle>
              <DialogDescription>
                Copy both values now. The verification secret cannot be shown again.
              </DialogDescription>
            </DialogHeader>
            <div className="rounded-lg border border-instrument-caution/40 bg-instrument-caution/5 p-4">
              <p className="text-sm font-medium">This secret is visible exactly once</p>
              <p className="mt-1 text-xs text-muted-foreground">
                Store it in the provider before closing this dialog. To replace a lost secret,
                create a new registration.
              </p>
            </div>
            <CopyValue
              label="Hook URL"
              value={hookPath}
              copied={copied === "url"}
              onCopy={() => copy("url", hookPath)}
            />
            <CopyValue
              label="Verification secret"
              value={created.secret}
              copied={copied === "secret"}
              onCopy={() => copy("secret", created.secret)}
            />
            <DialogFooter>
              <Button onClick={() => onOpenChange(false)}>I saved the secret</Button>
            </DialogFooter>
          </>
        ) : (
          <>
            <DialogHeader>
              <DialogTitle>Create webhook registration</DialogTitle>
              <DialogDescription>
                This creates one verified inbound endpoint. Automations choose which of its events
                launch a session.
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-4">
              <Field data-invalid={!!error && !id}>
                <FieldLabel htmlFor="registration-id">Slug ID</FieldLabel>
                <Input
                  id="registration-id"
                  value={id}
                  placeholder="linear-production"
                  onChange={(event) => setId(event.target.value.toLowerCase())}
                />
                <FieldDescription>
                  Lowercase letters, numbers, and hyphens; used in the URL.
                </FieldDescription>
              </Field>
              <Field>
                <FieldLabel htmlFor="registration-name">Name</FieldLabel>
                <Input
                  id="registration-name"
                  value={name}
                  placeholder="Linear production"
                  onChange={(event) => setName(event.target.value)}
                />
              </Field>
              <Field>
                <FieldLabel>Provider</FieldLabel>
                <Select value={providerHint || "generic"} onValueChange={onProviderChange}>
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="generic">Generic webhook</SelectItem>
                    {hints.map((hint) => (
                      <SelectItem key={hint.provider} value={hint.provider}>
                        {hint.name}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <FieldDescription>
                  Providers appear here only when their connector declares a webhook facet.
                </FieldDescription>
              </Field>
              <Field>
                <FieldLabel>Verification scheme</FieldLabel>
                <p className="text-sm text-muted-foreground">{SCHEME_LABELS[scheme]}</p>
                <FieldDescription>
                  GitHub, Slack, and Linear events arrive through their integrations: select them as
                  an automation trigger instead of registering a webhook.
                </FieldDescription>
              </Field>
              <FieldError>{error}</FieldError>
            </div>
            <DialogFooter>
              <Button
                onClick={onCreate}
                disabled={!id.trim() || !name.trim() || createRegistration.isPending}
              >
                {createRegistration.isPending ? "Creating…" : "Create webhook"}
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

function CopyValue({
  label,
  value,
  copied,
  onCopy,
}: {
  label: string;
  value: string;
  copied: boolean;
  onCopy: () => void;
}) {
  return (
    <Field>
      <FieldLabel>{label}</FieldLabel>
      <div className="flex gap-2">
        <code className="min-w-0 flex-1 overflow-x-auto rounded-md border bg-muted px-3 py-2 text-xs">
          {value}
        </code>
        <Button variant="outline" size="icon" aria-label={`Copy ${label}`} onClick={onCopy}>
          {copied ? <CheckIcon className="size-4" /> : <CopyIcon className="size-4" />}
        </Button>
      </div>
    </Field>
  );
}

function RegistrationRow({ registration }: { registration: WebhookRegistration }) {
  const remove = useDeleteWebhookRegistration();
  const [deleteError, setDeleteError] = useState("");
  const hookPath = `/api/v1/hooks/${registration.id}`;

  const onDelete = async () => {
    setDeleteError("");
    try {
      await remove.mutateAsync({ id: registration.id });
      toast.success("Webhook registration deleted");
    } catch (error) {
      const message = errorMessage(error);
      setDeleteError(message);
      toast.error(message);
    }
  };

  return (
    <div className="rounded-lg border bg-card p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="font-medium">{registration.name}</span>
            {registration.providerHint && (
              <Badge variant="secondary">{registration.providerHint}</Badge>
            )}
          </div>
          <p className="mt-1 font-mono text-xs text-muted-foreground">{hookPath}</p>
          <p className="mt-1 text-xs text-muted-foreground">
            {registration.verificationScheme === "generic_hmac_sha256"
              ? SCHEME_LABELS.generic_hmac_sha256
              : registration.verificationScheme}
          </p>
          {registration.disabledReason && (
            <p className="mt-2 rounded-md border border-destructive/40 bg-destructive/10 px-2 py-1 text-xs text-destructive">
              Retired: {registration.disabledReason}
            </p>
          )}
        </div>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button variant="ghost" size="sm">
              <Trash2Icon className="size-4" />
              Delete
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Delete “{registration.name}”?</AlertDialogTitle>
              <AlertDialogDescription>
                Its endpoint and verification secret will stop working. Bound automations must be
                archived or moved first.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Cancel</AlertDialogCancel>
              <AlertDialogAction onClick={onDelete}>Delete webhook</AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </div>
      {deleteError && <p className="mt-3 text-sm text-destructive">{deleteError}</p>}
    </div>
  );
}

export function WebhookRegistrationsPanel() {
  const registrations = useWebhookRegistrations();
  return (
    <section className="space-y-3 border-t pt-6" aria-labelledby="webhook-list-heading">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <div className="flex items-center gap-2">
            <WebhookIcon className="size-4 text-muted-foreground" />
            <h2 id="webhook-list-heading" className="font-semibold">
              Webhook registrations
            </h2>
          </div>
          <p className="mt-1 text-sm text-muted-foreground">
            Verified endpoints that can feed one or more automations.
          </p>
        </div>
        <CreateRegistrationDialog />
      </div>
      {registrations.isPending && <SkeletonRows rows={2} />}
      {registrations.error && (
        <EmptyState tone="error">{errorMessage(registrations.error)}</EmptyState>
      )}
      {!registrations.isPending && (registrations.data?.registrations.length ?? 0) === 0 && (
        <EmptyState>No registered webhook endpoints. Cron automations do not need one.</EmptyState>
      )}
      <div className="grid gap-3 lg:grid-cols-2">
        {registrations.data?.registrations.map((registration) => (
          <RegistrationRow key={registration.id} registration={registration} />
        ))}
      </div>
    </section>
  );
}
