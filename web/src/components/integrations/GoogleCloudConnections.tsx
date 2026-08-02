import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  CheckIcon,
  ChevronDownIcon,
  CloudIcon,
  PlusIcon,
  ShieldCheckIcon,
  Trash2Icon,
  TriangleAlertIcon,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  useCreateConnection,
  useDeleteConnection,
  useIntegrationConnections,
  useUpdateConnection,
} from "@/hooks/useIntegrations";
import { errorMessage } from "@/lib/errors";
import {
  ConnectionTestEnableControls,
  type ConnectionControlResult,
} from "./ConnectionTestEnableControls";
import type { IntegrationConnection } from "@/gen/engram/app/v1/integration_pb";

const GOOGLE_API_OPTIONS = [
  {
    host: "logging.googleapis.com",
    label: "Cloud Logging",
    description: "Read log entries and log metadata.",
  },
  {
    host: "monitoring.googleapis.com",
    label: "Cloud Monitoring",
    description: "Read metrics, time series, and monitoring metadata.",
  },
  {
    host: "cloudtrace.googleapis.com",
    label: "Cloud Trace",
    description: "Read distributed traces.",
  },
  {
    host: "container.googleapis.com",
    label: "Google Kubernetes Engine",
    description: "Read GKE cluster metadata and credentials.",
  },
  {
    host: "compute.googleapis.com",
    label: "Compute Engine",
    description: "Describe or operate Compute Engine instances.",
  },
  {
    host: "tunnel.cloudproxy.app",
    label: "IAP tunnelling",
    description: "Open an Identity-Aware Proxy TCP tunnel.",
  },
] as const;

const GOOGLE_API_HOSTS = new Set<string>(GOOGLE_API_OPTIONS.map((option) => option.host));

/**
 * Google's workload identity pool and provider ID rules (IAM
 * `workloadIdentityPools.create` / `…providers.create`): 4-32 characters from
 * `[a-z0-9-]`, and the `gcp-` prefix is reserved for Google.
 */
const WIF_ID_PATTERN = /^[a-z0-9-]{4,32}$/;
const isValidWifId = (value: string) => WIF_ID_PATTERN.test(value) && !value.startsWith("gcp-");
const ALIAS_PATTERN = /^[a-z][a-z0-9-]{1,62}$/;
const PROJECT_NUMBER_PATTERN = /^[0-9]+$/;
const HOST_PATTERN = /^(?=.{1,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/;

const splitHosts = (value: string) =>
  value
    .split(/[\s,]+/)
    .map((host) => host.trim().toLowerCase())
    .filter(Boolean);

/**
 * Endpoint selection shared by the connect and the endpoint-edit dialogs:
 * the curated checkboxes plus the free-form host textarea, merged and
 * validated the same way in both.
 */
function useEndpointSelection() {
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [custom, setCustom] = useState("");

  const toggle = useCallback(
    (host: string) =>
      setSelected((current) => {
        const next = new Set(current);
        if (next.has(host)) next.delete(host);
        else next.add(host);
        return next;
      }),
    [],
  );

  /** Seed from a connection's stored endpoints (endpoint-edit dialog).
   * Stable, so an open-dialog effect can depend on it. */
  const initialize = useCallback((endpoints: readonly string[]) => {
    setSelected(new Set(endpoints.filter((host) => GOOGLE_API_HOSTS.has(host))));
    setCustom(endpoints.filter((host) => !GOOGLE_API_HOSTS.has(host)).join("\n"));
  }, []);

  const clear = useCallback(() => {
    setSelected(new Set());
    setCustom("");
  }, []);

  const extra = splitHosts(custom);
  return {
    selected,
    custom,
    setCustom,
    toggle,
    initialize,
    clear,
    endpoints: [...new Set([...selected, ...extra])],
    customValid: extra.every((host) => HOST_PATTERN.test(host)),
  };
}

function GoogleApiOptions({
  selectedEndpoints,
  onToggle,
}: {
  selectedEndpoints: Set<string>;
  onToggle: (host: string) => void;
}) {
  return (
    <div className="grid gap-2 sm:grid-cols-2">
      {GOOGLE_API_OPTIONS.map((option) => {
        const selected = selectedEndpoints.has(option.host);
        return (
          <button
            key={option.host}
            type="button"
            role="checkbox"
            aria-checked={selected}
            className={`flex items-start gap-3 rounded-md border p-3 text-left transition-colors focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none ${
              selected ? "border-primary/60 bg-primary/[0.07]" : "border-border hover:bg-muted/40"
            }`}
            onClick={() => onToggle(option.host)}
          >
            <span
              className={`mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-sm border ${
                selected ? "border-primary bg-primary text-primary-foreground" : "border-input"
              }`}
            >
              {selected && <CheckIcon className="size-3" />}
            </span>
            <span>
              <span className="block text-sm font-medium">{option.label}</span>
              <span className="mt-0.5 block text-xs text-muted-foreground">
                {option.description}
              </span>
            </span>
          </button>
        );
      })}
    </div>
  );
}

export function connectionAlias(name: string): string {
  const slug = name
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 63)
    .replace(/-+$/g, "");
  if (/^[a-z]/.test(slug) && slug.length >= 2) return slug;
  const suffix = slug || "connection";
  return `gcp-${suffix}`.slice(0, 63).replace(/-+$/g, "");
}

function providerIdFor(alias: string): string {
  return `engrams-${alias}`.slice(0, 32).replace(/-+$/g, "");
}

export function GoogleCloudConnectDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const navigate = useNavigate();
  const create = useCreateConnection();
  const [displayName, setDisplayName] = useState("");
  const [alias, setAlias] = useState("");
  const [aliasEdited, setAliasEdited] = useState(false);
  const [projectNumber, setProjectNumber] = useState("");
  const [serviceAccount, setServiceAccount] = useState("");
  const endpointSelection = useEndpointSelection();
  const [poolId, setPoolId] = useState("engrams");
  const [providerId, setProviderId] = useState("");
  const [providerEdited, setProviderEdited] = useState(false);
  const [advancedOpen, setAdvancedOpen] = useState(false);

  const reset = () => {
    setDisplayName("");
    setAlias("");
    setAliasEdited(false);
    setProjectNumber("");
    setServiceAccount("");
    endpointSelection.clear();
    setPoolId("engrams");
    setProviderId("");
    setProviderEdited(false);
    setAdvancedOpen(false);
    create.reset();
  };

  const close = () => {
    onOpenChange(false);
    reset();
  };

  const handleNameChange = (name: string) => {
    setDisplayName(name);
    const nextAlias = connectionAlias(name);
    if (!aliasEdited) setAlias(nextAlias);
    if (!providerEdited) setProviderId(providerIdFor(aliasEdited ? alias : nextAlias));
  };

  const { endpoints } = endpointSelection;
  const formValid =
    displayName.trim().length > 0 &&
    ALIAS_PATTERN.test(alias) &&
    PROJECT_NUMBER_PATTERN.test(projectNumber) &&
    isValidWifId(poolId) &&
    isValidWifId(providerId) &&
    serviceAccount.trim().length > 0 &&
    endpoints.length > 0 &&
    endpointSelection.customValid;

  const add = async () => {
    // The failure path renders through `create.error` below; the try keeps the
    // mutateAsync rejection from escaping as an unhandled rejection.
    try {
      const response = await create.mutateAsync({
        alias,
        provider: "gcp",
        displayName: displayName.trim(),
        googleCloud: {
          workloadIdentityProvider:
            `//iam.googleapis.com/projects/${projectNumber}/locations/global/` +
            `workloadIdentityPools/${poolId}/providers/${providerId}`,
          serviceAccountEmail: serviceAccount.trim(),
          endpoints,
        },
      });
      if (!response.connection) return;
      const id = response.connection.id;
      close();
      await navigate({
        to: "/settings/integrations/$provider/$connectionId/setup",
        params: { provider: "gcp", connectionId: id },
      });
    } catch {
      // Shown via create.error.
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) close();
      }}
    >
      <DialogContent className="max-h-[90vh] max-w-3xl overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Connect Google Cloud</DialogTitle>
          <DialogDescription>
            Choose where sessions may act. Engrams generates the keyless Google Cloud setup next.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-6 py-1">
          <section className="space-y-3">
            <SectionHeading number="1" title="Connection" />
            <div className="grid gap-4 sm:grid-cols-2">
              <Field label="Connection name">
                <Input
                  aria-label="Connection name"
                  value={displayName}
                  onChange={(event) => handleNameChange(event.target.value)}
                  placeholder="Production observer"
                />
                <p className="mt-1.5 text-xs text-muted-foreground">
                  Stable alias: <code>{alias || "generated-from-name"}</code>
                </p>
              </Field>
              <Field label="Google Cloud project number">
                <Input
                  aria-label="Google Cloud project number"
                  inputMode="numeric"
                  value={projectNumber}
                  onChange={(event) => setProjectNumber(event.target.value.trim())}
                  placeholder="123456789012"
                />
                <p className="mt-1.5 text-xs text-muted-foreground">
                  Find it with{" "}
                  <code>
                    gcloud projects describe PROJECT_ID --format=&apos;value(projectNumber)&apos;
                  </code>
                  .
                </p>
              </Field>
              <div className="sm:col-span-2">
                <Field label="Service account email">
                  <Input
                    aria-label="Service account email"
                    value={serviceAccount}
                    onChange={(event) => setServiceAccount(event.target.value)}
                    placeholder="engrams-reader@project.iam.gserviceaccount.com"
                  />
                  <p className="mt-1.5 text-xs text-muted-foreground">
                    Google IAM roles on this account control what sessions may do.
                  </p>
                </Field>
              </div>
            </div>
          </section>

          <section className="space-y-3">
            <div>
              <SectionHeading number="2" title="Allowed APIs" />
              <p className="mt-1 text-xs text-muted-foreground">
                Select only the Google services that this connection needs.
              </p>
            </div>
            <GoogleApiOptions
              selectedEndpoints={endpointSelection.selected}
              onToggle={endpointSelection.toggle}
            />
          </section>

          <Collapsible open={advancedOpen} onOpenChange={setAdvancedOpen}>
            <CollapsibleTrigger asChild>
              <Button variant="ghost" className="w-full justify-between px-0 hover:bg-transparent">
                Advanced settings
                <ChevronDownIcon
                  className={`size-4 transition-transform ${advancedOpen ? "rotate-180" : ""}`}
                />
              </Button>
            </CollapsibleTrigger>
            <CollapsibleContent className="space-y-4 border-t pt-4">
              <div className="grid gap-4 sm:grid-cols-3">
                <Field label="Connection alias">
                  <Input
                    aria-label="Connection alias"
                    value={alias}
                    onChange={(event) => {
                      setAliasEdited(true);
                      setAlias(event.target.value);
                      if (!providerEdited) setProviderId(providerIdFor(event.target.value));
                    }}
                  />
                </Field>
                <Field label="WIF pool ID">
                  <Input
                    aria-label="WIF pool ID"
                    value={poolId}
                    onChange={(event) => setPoolId(event.target.value)}
                  />
                  <WifIdHint value={poolId} />
                </Field>
                <Field label="WIF provider ID">
                  <Input
                    aria-label="WIF provider ID"
                    value={providerId}
                    onChange={(event) => {
                      setProviderEdited(true);
                      setProviderId(event.target.value);
                    }}
                  />
                  <WifIdHint value={providerId} />
                </Field>
              </div>
              <CustomEndpointsField
                value={endpointSelection.custom}
                onChange={endpointSelection.setCustom}
              />
            </CollapsibleContent>
          </Collapsible>
        </div>

        {create.error && <p className="text-sm text-destructive">{errorMessage(create.error)}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={close}>
            Cancel
          </Button>
          <Button disabled={create.isPending || !formValid} onClick={() => void add()}>
            Create and continue
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export function GoogleCloudEndpointDialog({
  connection,
  open,
  onOpenChange,
  onSaved,
}: {
  connection: IntegrationConnection;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSaved?: () => void;
}) {
  const update = useUpdateConnection();
  const endpointSelection = useEndpointSelection();
  const { initialize } = endpointSelection;

  const storedEndpoints = connection.googleCloud?.endpoints;
  useEffect(() => {
    if (!open) return;
    initialize(storedEndpoints ?? []);
  }, [storedEndpoints, open, initialize]);

  const { endpoints } = endpointSelection;
  const google = connection.googleCloud;
  const formValid = Boolean(google) && endpoints.length > 0 && endpointSelection.customValid;

  const save = async () => {
    if (!google) return;
    // The failure path renders through `update.error` below; the try keeps the
    // mutateAsync rejection from escaping as an unhandled rejection.
    try {
      await update.mutateAsync({
        id: connection.id,
        alias: connection.alias,
        displayName: connection.displayName,
        googleCloud: {
          workloadIdentityProvider: google.workloadIdentityProvider,
          serviceAccountEmail: google.serviceAccountEmail,
          endpoints,
        },
      });
    } catch {
      // Shown via update.error.
      return;
    }
    onOpenChange(false);
    onSaved?.();
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) update.reset();
        onOpenChange(next);
      }}
    >
      <DialogContent className="max-h-[90vh] max-w-3xl overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Edit allowed Google Cloud APIs</DialogTitle>
          <DialogDescription>
            Limit new sessions to the exact service endpoints that this connection needs.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-1">
          <GoogleApiOptions
            selectedEndpoints={endpointSelection.selected}
            onToggle={endpointSelection.toggle}
          />
          <CustomEndpointsField
            value={endpointSelection.custom}
            onChange={endpointSelection.setCustom}
          />
          <p className="rounded-md border border-amber-500/30 bg-amber-500/[0.07] p-3 text-xs text-muted-foreground">
            Saving disables this connection and clears its test result. Test and enable it again
            before you launch a new session. Existing sessions keep their stamped configuration.
          </p>
        </div>

        {update.error && <p className="text-sm text-destructive">{errorMessage(update.error)}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button disabled={update.isPending || !formValid} onClick={() => void save()}>
            Save API changes
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export function GoogleCloudConnections() {
  const { data, isLoading } = useIntegrationConnections();
  const remove = useDeleteConnection();
  const [adding, setAdding] = useState(false);
  const [confirmingDeleteId, setConfirmingDeleteId] = useState<string | null>(null);
  const [rowResults, setRowResults] = useState<Record<string, { ok: boolean; message: string }>>(
    {},
  );
  const setRowResult = (id: string, ok: boolean, message: string) =>
    setRowResults((current) => ({ ...current, [id]: { ok, message } }));
  const clearRowResult = (id: string) =>
    setRowResults((current) => {
      const { [id]: _dropped, ...rest } = current;
      return rest;
    });
  const connections = (data?.connections ?? []).filter(
    (connection) => connection.provider === "gcp",
  );

  const onControlResult = (id: string, result: ConnectionControlResult) => {
    if (result.kind === "enable" && result.ok) {
      clearRowResult(id);
      return;
    }
    setRowResult(id, result.ok, result.message);
  };

  const deleteConnection = (connection: IntegrationConnection) => {
    remove.mutate(
      { id: connection.id },
      {
        onSuccess: () => {
          setConfirmingDeleteId(null);
          toast.success(`${connection.displayName} deleted`);
        },
        // The server rejects the delete while a profile still grants this
        // connection's powers — keep the confirm bar open and show why.
        onError: (error) => setRowResult(connection.id, false, errorMessage(error)),
      },
    );
  };

  return (
    <section className="overflow-hidden rounded-lg border bg-card">
      <div className="flex flex-wrap items-start justify-between gap-4 border-b p-5">
        <div className="max-w-2xl">
          <div className="flex items-center gap-2">
            <span className="flex size-8 items-center justify-center rounded-md bg-blue-600 text-white">
              <CloudIcon className="size-4" />
            </span>
            <div>
              <h2 className="text-sm font-semibold">Google Cloud</h2>
              <p className="text-xs text-muted-foreground">Workload Identity Federation only</p>
            </div>
          </div>
          <div className="mt-4 flex flex-wrap items-center gap-2 font-mono text-[11px] text-muted-foreground">
            <span className="rounded border px-2 py-1">Engrams session</span>
            <span aria-hidden>→</span>
            <span className="rounded border px-2 py-1">Customer WIF provider</span>
            <span aria-hidden>→</span>
            <span className="rounded border px-2 py-1">One service account</span>
          </div>
        </div>
        <Button size="sm" onClick={() => setAdding(true)}>
          <PlusIcon className="size-3.5" /> Add connection
        </Button>
      </div>

      <div className="divide-y">
        {isLoading && <p className="p-5 text-sm text-muted-foreground">Loading connections…</p>}
        {!isLoading && connections.length === 0 && (
          <p className="p-5 text-sm text-muted-foreground">
            Add a connection to generate the customer-side WIF configuration. No key file is
            accepted.
          </p>
        )}
        {connections.map((connection) => {
          const rowResult = rowResults[connection.id];
          return (
            <div key={connection.id} className="flex flex-col gap-3 p-5">
              <div className="flex flex-wrap items-center gap-4">
                <ShieldCheckIcon className="size-4 text-muted-foreground" />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="text-sm font-medium">{connection.displayName}</span>
                    <Badge variant={connection.enabled ? "default" : "secondary"}>
                      {connection.enabled
                        ? "Enabled"
                        : connection.testedAt
                          ? "Tested"
                          : "Setup required"}
                    </Badge>
                  </div>
                  <p className="mt-1 truncate font-mono text-xs text-muted-foreground">
                    {connection.alias} · {connection.googleCloud?.serviceAccountEmail}
                  </p>
                </div>
                <div className="flex items-center gap-2">
                  <Button asChild variant="outline" size="sm">
                    <Link
                      to="/settings/integrations/$provider/$connectionId/setup"
                      params={{ provider: connection.provider, connectionId: connection.id }}
                    >
                      Setup
                    </Link>
                  </Button>
                  <ConnectionTestEnableControls
                    connection={connection}
                    compact
                    onResult={(result) => onControlResult(connection.id, result)}
                  />
                  <Button
                    variant="ghost"
                    size="icon"
                    aria-label={`Delete ${connection.displayName}`}
                    onClick={() => {
                      clearRowResult(connection.id);
                      setConfirmingDeleteId(connection.id);
                    }}
                  >
                    <Trash2Icon className="size-4" />
                  </Button>
                </div>
              </div>
              {confirmingDeleteId === connection.id && (
                <div className="flex flex-wrap items-center gap-3 rounded-lg border border-destructive/40 bg-destructive/5 p-3">
                  <TriangleAlertIcon className="size-4 shrink-0 text-destructive" />
                  <span className="min-w-0 flex-1 text-sm">
                    Delete "{connection.displayName}"? Sessions can no longer mint its credential.
                    Remove its grants from profiles first.
                  </span>
                  <Button variant="ghost" size="sm" onClick={() => setConfirmingDeleteId(null)}>
                    Cancel
                  </Button>
                  <Button
                    variant="destructive"
                    size="sm"
                    disabled={remove.isPending}
                    onClick={() => deleteConnection(connection)}
                  >
                    Delete
                  </Button>
                </div>
              )}
              {rowResult && (
                <p
                  role="status"
                  className={`text-xs ${rowResult.ok ? "text-muted-foreground" : "text-destructive"}`}
                >
                  {rowResult.message}
                </p>
              )}
            </div>
          );
        })}
      </div>

      <GoogleCloudConnectDialog open={adding} onOpenChange={setAdding} />
    </section>
  );
}

function SectionHeading({ number, title }: { number: string; title: string }) {
  return (
    <div className="flex items-center gap-2">
      <span className="flex size-5 items-center justify-center rounded-full border font-mono text-[10px] text-muted-foreground">
        {number}
      </span>
      <h3 className="text-sm font-semibold">{title}</h3>
    </div>
  );
}

function CustomEndpointsField({
  value,
  onChange,
}: {
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <Field label="Other allowed API hostnames">
      <Textarea
        aria-label="Other allowed API hostnames"
        value={value}
        onChange={(event) => onChange(event.target.value)}
        rows={3}
        placeholder="artifactregistry.googleapis.com"
      />
      <p className="mt-1.5 text-xs text-muted-foreground">
        Enter exact hostnames only. Google STS and OAuth endpoints are always blocked.
      </p>
    </Field>
  );
}

function WifIdHint({ value }: { value: string }) {
  if (!value || isValidWifId(value)) return null;
  return (
    <p className="mt-1.5 text-xs text-destructive">
      Use 4-32 characters from a-z, 0-9, and hyphens. Google reserves the "gcp-" prefix.
    </p>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="space-y-1.5 text-xs font-medium">
      {label}
      {children}
    </label>
  );
}
