import { useMemo, useState } from "react";
import { Link } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  ArchiveIcon,
  CheckIcon,
  Clock3Icon,
  CopyIcon,
  PlusIcon,
  RadioTowerIcon,
  Trash2Icon,
  WebhookIcon,
} from "lucide-react";

import type { Automation, WebhookRegistration } from "@/gen/engram/app/v1/automation_pb";
import { useConnectors } from "@/hooks/useIntegrations";
import { useProfiles } from "@/hooks/useProfiles";
import {
  useArchiveAutomation,
  useAutomationRuns,
  useAutomations,
  useCreateWebhookRegistration,
  useDeleteWebhookRegistration,
  useSetAutomationEnabled,
  useWebhookRegistrations,
} from "@/hooks/useAutomations";
import {
  automationStatusLabel,
  webhookConnectorHints,
  type VerificationScheme,
} from "@/lib/automations";
import { errorMessage } from "@/lib/errors";
import { PageHeading } from "@/components/page-heading";
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
import { Switch } from "@/components/ui/switch";

function dateTime(value: string | undefined): string {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

/** The automation's harness/model/effort override, or "" when it inherits the
 *  profile's default (ADR 0063 B2). Ids, not labels — the descriptor labels are
 *  not loaded on the list page. */
function overrideSummary(automation: Automation): string {
  const action = automation.action?.action;
  if (action?.case !== "createTask") return "";
  return [action.value.harness, action.value.model, action.value.effort]
    .filter((part) => !!part)
    .join(" · ");
}

function StatusBadge({ status }: { status: string }) {
  const variant =
    status === "launched"
      ? "default"
      : status === "skipped"
        ? "secondary"
        : status
          ? "destructive"
          : "outline";
  return <Badge variant={variant}>{status ? automationStatusLabel(status) : "never run"}</Badge>;
}

function AutomationRow({
  automation,
  profileName,
}: {
  automation: Automation;
  profileName: string;
}) {
  const runs = useAutomationRuns(automation.id, 1);
  const setEnabled = useSetAutomationEnabled();
  const archive = useArchiveAutomation();
  const trigger = automation.trigger?.trigger;
  const override = overrideSummary(automation);
  const summary =
    trigger?.case === "cron"
      ? `${trigger.value.schedule} · ${trigger.value.timezone}`
      : trigger?.case === "webhook"
        ? `${trigger.value.registrationId} · ${trigger.value.events.join(", ")}`
        : "Trigger not configured";

  const onEnabledChange = async (enabled: boolean) => {
    try {
      await setEnabled.mutateAsync({ id: automation.id, enabled });
      toast.success(enabled ? "Automation enabled" : "Automation paused");
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  const onArchive = async () => {
    try {
      await archive.mutateAsync({ id: automation.id });
      toast.success("Automation archived");
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  return (
    <div className="grid gap-4 rounded-lg border bg-card p-4 shadow-xs md:grid-cols-[minmax(0,1fr)_auto] md:items-center">
      <Link
        to="/settings/automations/$id"
        params={{ id: automation.id }}
        className="group min-w-0 outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
      >
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-display text-base font-semibold group-hover:underline">
            {automation.name}
          </span>
          <StatusBadge status={runs.data?.runs[0]?.status ?? ""} />
        </div>
        {automation.description && (
          <p className="mt-1 truncate text-sm text-muted-foreground">{automation.description}</p>
        )}
        <div className="mt-3 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
          <span className="inline-flex min-w-0 items-center gap-1.5 font-mono">
            {trigger?.case === "cron" ? (
              <Clock3Icon className="size-3.5 shrink-0" />
            ) : (
              <WebhookIcon className="size-3.5 shrink-0" />
            )}
            <span className="truncate">{summary}</span>
          </span>
          <span>Profile: {profileName}</span>
          {override && <span>Runs on: {override}</span>}
          {automation.nextFireAt && <span>Next: {dateTime(automation.nextFireAt)}</span>}
        </div>
      </Link>
      <div className="flex items-center justify-between gap-2 md:justify-end">
        <label className="inline-flex items-center gap-2 text-sm">
          <Switch
            aria-label={`${automation.enabled ? "Disable" : "Enable"} ${automation.name}`}
            checked={automation.enabled}
            disabled={setEnabled.isPending}
            onCheckedChange={onEnabledChange}
          />
          {automation.enabled ? "Enabled" : "Paused"}
        </label>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button variant="ghost" size="sm">
              <ArchiveIcon className="size-4" />
              Archive
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Archive “{automation.name}”?</AlertDialogTitle>
              <AlertDialogDescription>
                It will stop receiving webhook events or scheduled fires. Run history remains
                available in the database.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Cancel</AlertDialogCancel>
              <AlertDialogAction onClick={onArchive}>Archive automation</AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </div>
    </div>
  );
}

const SCHEME_LABELS: Record<VerificationScheme, string> = {
  generic_hmac_sha256: "Generic HMAC SHA-256",
  github_hmac_sha256: "GitHub HMAC SHA-256",
  slack_v0: "Slack v0 signature",
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
    const hint = hints.find((item) => item.provider === value);
    if (hint) setScheme(hint.verificationScheme);
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
                <Select
                  value={scheme}
                  onValueChange={(value) => setScheme(value as VerificationScheme)}
                  disabled={!!providerHint}
                >
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {Object.entries(SCHEME_LABELS).map(([value, label]) => (
                      <SelectItem key={value} value={value}>
                        {label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
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
            {SCHEME_LABELS[registration.verificationScheme as VerificationScheme] ??
              registration.verificationScheme}
          </p>
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

export function Automations() {
  const automations = useAutomations();
  const registrations = useWebhookRegistrations();
  const profiles = useProfiles(true);
  const profileNames = new Map(
    (profiles.data?.profiles ?? []).map((profile) => [profile.id, profile.name]),
  );

  return (
    <div className="space-y-8">
      <PageHeading
        title="Automations"
        eyebrow="Org · Unattended launches"
        description="Turn schedules and verified webhook events into sessions. Each automation stays inspectable: one trigger, one profile, one rendered prompt."
        actions={
          <Button asChild size="sm">
            <Link to="/settings/automations/new">
              <PlusIcon className="size-3.5" />
              New automation
            </Link>
          </Button>
        }
      />

      <section className="space-y-3" aria-labelledby="automation-list-heading">
        <div className="flex items-center gap-2">
          <RadioTowerIcon className="size-4 text-muted-foreground" />
          <h2 id="automation-list-heading" className="font-display font-semibold">
            Active automations
          </h2>
          <span className="font-mono text-xs text-muted-foreground">
            {automations.data?.automations.length ?? 0}
          </span>
        </div>
        {automations.isPending && <p className="text-sm text-muted-foreground">Loading…</p>}
        {automations.error && (
          <p className="text-sm text-destructive">{errorMessage(automations.error)}</p>
        )}
        {!automations.isPending && (automations.data?.automations.length ?? 0) === 0 && (
          <div className="rounded-lg border border-dashed p-8 text-center">
            <p className="text-sm text-muted-foreground">No automations yet</p>
            <Button asChild className="mt-3">
              <Link to="/settings/automations/new">Create automation</Link>
            </Button>
          </div>
        )}
        <div className="space-y-3">
          {automations.data?.automations.map((automation) => {
            const profileId =
              automation.action?.action.case === "createTask"
                ? automation.action.action.value.profileId
                : "";
            return (
              <AutomationRow
                key={automation.id}
                automation={automation}
                profileName={profileNames.get(profileId) ?? profileId ?? "—"}
              />
            );
          })}
        </div>
      </section>

      <section className="space-y-3 border-t pt-6" aria-labelledby="webhook-list-heading">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <div className="flex items-center gap-2">
              <WebhookIcon className="size-4 text-muted-foreground" />
              <h2 id="webhook-list-heading" className="font-display font-semibold">
                Webhook registrations
              </h2>
            </div>
            <p className="mt-1 text-sm text-muted-foreground">
              Verified endpoints that can feed one or more automations.
            </p>
          </div>
          <CreateRegistrationDialog />
        </div>
        {registrations.isPending && <p className="text-sm text-muted-foreground">Loading…</p>}
        {registrations.error && (
          <p className="text-sm text-destructive">{errorMessage(registrations.error)}</p>
        )}
        {!registrations.isPending && (registrations.data?.registrations.length ?? 0) === 0 && (
          <div className="rounded-lg border border-dashed p-6 text-sm text-muted-foreground">
            No registered webhook endpoints. Cron automations do not need one.
          </div>
        )}
        <div className="grid gap-3 lg:grid-cols-2">
          {registrations.data?.registrations.map((registration) => (
            <RegistrationRow key={registration.id} registration={registration} />
          ))}
        </div>
      </section>
    </div>
  );
}
