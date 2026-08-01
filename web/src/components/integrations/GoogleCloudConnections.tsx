import { useState } from "react";
import {
  CheckCircle2Icon,
  CloudIcon,
  CopyIcon,
  PlusIcon,
  ShieldCheckIcon,
  Trash2Icon,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
  useGoogleCloudSetup,
  useIntegrationConnections,
  useSetConnectionEnabled,
  useTestConnection,
} from "@/hooks/useIntegrations";
import type { IntegrationConnection } from "@/gen/engram/app/v1/integration_pb";

const splitHosts = (value: string) =>
  value
    .split(/[\s,]+/)
    .map((host) => host.trim().toLowerCase())
    .filter(Boolean);

export function GoogleCloudConnections() {
  const { data, isLoading } = useIntegrationConnections();
  const create = useCreateConnection();
  const remove = useDeleteConnection();
  const test = useTestConnection();
  const enable = useSetConnectionEnabled();
  const setup = useGoogleCloudSetup();
  const [adding, setAdding] = useState(false);
  const [setupFor, setSetupFor] = useState<IntegrationConnection | null>(null);
  const [alias, setAlias] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [provider, setProvider] = useState("");
  const [serviceAccount, setServiceAccount] = useState("");
  const [endpoints, setEndpoints] = useState(
    "compute.googleapis.com\nlogging.googleapis.com\ncloudtrace.googleapis.com\ncontainer.googleapis.com\ntunnel.cloudproxy.app",
  );
  const [result, setResult] = useState<string | null>(null);
  const connections = (data?.connections ?? []).filter(
    (connection) => connection.provider === "gcp",
  );

  const openSetup = async (connection: IntegrationConnection) => {
    setSetupFor(connection);
    setResult(null);
    await setup.mutateAsync({ id: connection.id });
  };

  const add = async () => {
    const response = await create.mutateAsync({
      alias: alias.trim(),
      provider: "gcp",
      displayName: displayName.trim(),
      googleCloud: {
        workloadIdentityProvider: provider.trim(),
        serviceAccountEmail: serviceAccount.trim(),
        endpoints: splitHosts(endpoints),
      },
    });
    setAdding(false);
    setAlias("");
    setDisplayName("");
    setProvider("");
    setServiceAccount("");
    if (response.connection) await openSetup(response.connection);
  };

  const copy = (value: string) => navigator.clipboard.writeText(value);

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
        {connections.map((connection) => (
          <div key={connection.id} className="flex flex-wrap items-center gap-4 p-5">
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
              <Button variant="outline" size="sm" onClick={() => void openSetup(connection)}>
                Setup
              </Button>
              <Button
                variant="outline"
                size="sm"
                disabled={test.isPending}
                onClick={async () => {
                  const response = await test.mutateAsync({ id: connection.id });
                  setResult(response.message);
                }}
              >
                Test
              </Button>
              <Button
                size="sm"
                variant={connection.enabled ? "outline" : "default"}
                disabled={!connection.enabled && !connection.testedAt}
                onClick={() => enable.mutate({ id: connection.id, enabled: !connection.enabled })}
              >
                {connection.enabled ? "Disable" : "Enable"}
              </Button>
              <Button
                variant="ghost"
                size="icon"
                aria-label={`Delete ${connection.displayName}`}
                onClick={() => remove.mutate({ id: connection.id })}
              >
                <Trash2Icon className="size-4" />
              </Button>
            </div>
          </div>
        ))}
      </div>
      {result && <p className="border-t px-5 py-3 text-xs text-muted-foreground">{result}</p>}

      <Dialog open={adding} onOpenChange={(open) => !open && setAdding(false)}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>Add Google Cloud connection</DialogTitle>
            <DialogDescription>
              Enter public WIF coordinates. Engrams does not accept service-account keys or
              deployment credentials.
            </DialogDescription>
          </DialogHeader>
          <div className="grid gap-4 py-2 sm:grid-cols-2">
            <Field label="Connection alias">
              <Input
                aria-label="Connection alias"
                value={alias}
                onChange={(e) => setAlias(e.target.value)}
                placeholder="prod-readonly"
              />
            </Field>
            <Field label="Display name">
              <Input
                aria-label="Display name"
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
                placeholder="Production read only"
              />
            </Field>
            <div className="sm:col-span-2">
              <Field label="Workload identity provider resource">
                <Input
                  aria-label="Workload identity provider resource"
                  value={provider}
                  onChange={(e) => setProvider(e.target.value)}
                  placeholder="//iam.googleapis.com/projects/123…/providers/engrams"
                />
              </Field>
            </div>
            <div className="sm:col-span-2">
              <Field label="Target service-account email">
                <Input
                  aria-label="Target service-account email"
                  value={serviceAccount}
                  onChange={(e) => setServiceAccount(e.target.value)}
                  placeholder="engrams-reader@project.iam.gserviceaccount.com"
                />
              </Field>
            </div>
            <div className="sm:col-span-2">
              <Field label="Allowed API endpoints">
                <Textarea
                  aria-label="Allowed API endpoints"
                  value={endpoints}
                  onChange={(e) => setEndpoints(e.target.value)}
                  rows={4}
                />
                <p className="mt-1.5 text-xs text-muted-foreground">
                  Exact hostnames only. Remove APIs this connection does not need; keep
                  tunnel.cloudproxy.app only when the profile can open IAP tunnels.
                </p>
              </Field>
            </div>
          </div>
          {create.error && <p className="text-sm text-destructive">{String(create.error)}</p>}
          <DialogFooter>
            <Button variant="outline" onClick={() => setAdding(false)}>
              Cancel
            </Button>
            <Button
              disabled={create.isPending || !alias || !displayName || !provider || !serviceAccount}
              onClick={() => void add()}
            >
              Create connection
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={setupFor !== null} onOpenChange={(open) => !open && setSetupFor(null)}>
        <DialogContent className="max-h-[85vh] max-w-3xl overflow-y-auto">
          <DialogHeader>
            <DialogTitle>Configure {setupFor?.displayName}</DialogTitle>
            <DialogDescription>
              Apply one setup form, then test and enable the connection.
            </DialogDescription>
          </DialogHeader>
          {setup.data && (
            <div className="space-y-5">
              <CodeBlock label="Terraform" value={setup.data.terraform} onCopy={copy} />
              <CodeBlock label="gcloud" value={setup.data.gcloudScript} onCopy={copy} />
              <div className="flex items-center gap-2 text-xs text-muted-foreground">
                <CheckCircle2Icon className="size-4" /> Allowed audience:{" "}
                <code>{setup.data.audience}</code>
              </div>
            </div>
          )}
          {setup.isPending && <p className="text-sm text-muted-foreground">Generating setup…</p>}
          <DialogFooter>
            <Button variant="outline" onClick={() => setSetupFor(null)}>
              Close
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
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

function CodeBlock({
  label,
  value,
  onCopy,
}: {
  label: string;
  value: string;
  onCopy: (value: string) => void;
}) {
  return (
    <div>
      <div className="mb-2 flex items-center justify-between">
        <span className="text-xs font-medium">{label}</span>
        <Button variant="ghost" size="sm" onClick={() => void onCopy(value)}>
          <CopyIcon className="size-3.5" /> Copy
        </Button>
      </div>
      <pre className="overflow-x-auto rounded-md border bg-muted/40 p-3 text-[11px] leading-5">
        <code>{value}</code>
      </pre>
    </div>
  );
}
