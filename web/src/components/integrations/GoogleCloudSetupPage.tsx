import { lazy, Suspense, useState } from "react";
import { Link, useParams } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  CheckCircle2Icon,
  ChevronLeftIcon,
  CircleIcon,
  CloudIcon,
  CopyIcon,
  DownloadIcon,
  Loader2Icon,
  ShieldCheckIcon,
  TerminalIcon,
  TriangleAlertIcon,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useGoogleCloudSetup, useIntegrationConnections } from "@/hooks/useIntegrations";
import { errorMessage } from "@/lib/errors";
import {
  ConnectionTestEnableControls,
  type ConnectionControlResult,
} from "./ConnectionTestEnableControls";
import { GoogleCloudEndpointDialog } from "./GoogleCloudConnections";
import type { SetupCodeLanguage } from "./SyntaxHighlightedCode";

const SyntaxHighlightedCode = lazy(() => import("./SyntaxHighlightedCode"));

export function GoogleCloudSetupPage() {
  const { connectionId } = useParams({ strict: false }) as { connectionId?: string };
  return <GoogleCloudSetupWorkspace connectionId={connectionId ?? ""} />;
}

export function GoogleCloudSetupWorkspace({ connectionId }: { connectionId: string }) {
  const connections = useIntegrationConnections();
  const setup = useGoogleCloudSetup(connectionId);
  const [testResult, setTestResult] = useState<{ ok: boolean; message: string } | null>(null);
  const [editingEndpoints, setEditingEndpoints] = useState(false);
  const connection = connections.data?.connections.find((entry) => entry.id === connectionId);

  if (connections.isLoading) {
    return <p className="py-6 text-sm text-muted-foreground">Loading Google Cloud setup…</p>;
  }
  if (!connection || connection.provider !== "gcp") {
    return (
      <div className="mx-auto max-w-3xl">
        <BackToGoogleCloud />
        <p className="mt-4 text-sm text-muted-foreground">Google Cloud connection not found.</p>
      </div>
    );
  }

  const tested = Boolean(connection.testedAt) || testResult?.ok === true;
  const enabled = connection.enabled;

  const onControlResult = (result: ConnectionControlResult) => {
    if (result.kind === "test") {
      setTestResult({ ok: result.ok, message: result.message });
      if (result.ok) toast.success("Google Cloud connection verified");
      return;
    }
    if (!result.ok) {
      toast.error(result.message);
      return;
    }
    toast.success(
      result.enabled ? "Google Cloud connection enabled" : "Google Cloud connection disabled",
    );
  };

  return (
    <main className="mx-auto flex w-full max-w-[96rem] flex-col gap-5">
      <BackToGoogleCloud />

      <header className="flex flex-wrap items-start justify-between gap-4 border-b pb-5">
        <div className="flex min-w-0 items-start gap-3">
          <span className="flex size-10 shrink-0 items-center justify-center rounded-lg bg-blue-600 text-white">
            <CloudIcon className="size-5" />
          </span>
          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-2">
              <h1 className="text-2xl font-semibold">Set up {connection.displayName}</h1>
              <Badge variant={enabled ? "default" : "secondary"}>
                {enabled ? "Enabled" : tested ? "Tested" : "Setup required"}
              </Badge>
            </div>
            <p className="mt-1 text-sm text-muted-foreground">
              Apply one configuration in Google Cloud, then verify and enable this connection.
            </p>
          </div>
        </div>
      </header>

      <div className="grid min-w-0 gap-5 lg:grid-cols-[18rem_minmax(0,1fr)]">
        <aside className="space-y-4">
          <section className="rounded-lg border bg-card p-4">
            <h2 className="text-xs font-semibold text-muted-foreground">Progress</h2>
            <ol className="mt-4 space-y-4">
              <ProgressStep complete title="Connection created" detail={connection.alias} />
              <ProgressStep
                complete={tested}
                active={!tested}
                title="Apply Google setup"
                detail="Terraform or gcloud"
              />
              <ProgressStep
                complete={enabled}
                active={tested && !enabled}
                title="Test and enable"
                detail={enabled ? "Ready for profiles" : "Verify WIF exchange"}
              />
            </ol>
          </section>

          <section className="rounded-lg border bg-card p-4">
            <div className="flex items-center justify-between gap-3">
              <h2 className="text-xs font-semibold text-muted-foreground">Connection boundary</h2>
              <Button variant="ghost" size="sm" onClick={() => setEditingEndpoints(true)}>
                Edit APIs
              </Button>
            </div>
            <dl className="mt-3 space-y-3 text-xs">
              <div>
                <dt className="text-muted-foreground">Service account</dt>
                <dd className="mt-1 break-all font-mono">
                  {connection.googleCloud?.serviceAccountEmail}
                </dd>
              </div>
              <div>
                <dt className="text-muted-foreground">Allowed APIs</dt>
                <dd className="mt-2 flex flex-wrap gap-1.5">
                  {connection.googleCloud?.endpoints.map((endpoint) => (
                    <span
                      key={endpoint}
                      className="rounded border px-1.5 py-1 font-mono text-[10px]"
                    >
                      {endpoint}
                    </span>
                  ))}
                </dd>
              </div>
              {connection.googleCloud?.cloudSqlPostgresInstance && (
                <div>
                  <dt className="text-muted-foreground">Cloud SQL PostgreSQL</dt>
                  <dd className="mt-1 break-all font-mono">
                    {connection.googleCloud.cloudSqlPostgresInstance}
                  </dd>
                </div>
              )}
            </dl>
          </section>
        </aside>

        <section className="min-w-0 overflow-hidden rounded-lg border bg-card">
          <div className="border-b px-5 py-4">
            <h2 className="text-sm font-semibold">Apply in Google Cloud</h2>
            <p className="mt-1 text-xs text-muted-foreground">
              Choose the setup format that matches how you manage your infrastructure.
            </p>
          </div>

          {setup.isLoading && (
            <div className="flex items-center gap-2 p-5 text-sm text-muted-foreground">
              <Loader2Icon className="size-4 animate-spin" /> Generating setup…
            </div>
          )}
          {setup.error && (
            <div className="m-5 flex items-start gap-2 rounded-md border border-destructive/45 bg-destructive/[0.06] p-3 text-sm">
              <TriangleAlertIcon className="mt-0.5 size-4 shrink-0 text-destructive" />
              <span>{errorMessage(setup.error)}</span>
            </div>
          )}
          {setup.data && (
            <Tabs defaultValue="terraform" className="min-w-0 gap-0">
              <div className="flex flex-wrap items-center justify-between gap-3 border-b px-5 py-2">
                <TabsList variant="line">
                  <TabsTrigger value="terraform">Terraform</TabsTrigger>
                  <TabsTrigger value="gcloud">
                    <TerminalIcon className="size-3.5" /> gcloud
                  </TabsTrigger>
                </TabsList>
              </div>
              <TabsContent value="terraform" className="min-w-0 p-5">
                <SetupCode
                  label="Terraform configuration"
                  filename={`${connection.alias}-wif.tf`}
                  language="terraform"
                  value={setup.data.terraform}
                />
              </TabsContent>
              <TabsContent value="gcloud" className="min-w-0 p-5">
                <SetupCode
                  label="gcloud commands"
                  filename={`${connection.alias}-wif.sh`}
                  language="shellscript"
                  value={setup.data.gcloudScript}
                />
              </TabsContent>
              <Collapsible>
                <div className="border-t px-5 py-3">
                  <CollapsibleTrigger className="text-xs text-muted-foreground underline-offset-4 hover:underline">
                    Show federation details
                  </CollapsibleTrigger>
                  <CollapsibleContent className="mt-3 grid gap-3 text-xs sm:grid-cols-2">
                    <Detail label="Issuer" value={setup.data.issuer} />
                    <Detail label="Allowed audience" value={setup.data.audience} />
                  </CollapsibleContent>
                </div>
              </Collapsible>
            </Tabs>
          )}

          <div className="flex flex-wrap items-center justify-between gap-3 border-t bg-muted/20 px-5 py-4">
            <div>
              <p className="text-sm font-medium">Verify the applied configuration</p>
              <p className="mt-0.5 text-xs text-muted-foreground">
                The test exchanges an Engrams OIDC token and impersonates the service account.
              </p>
            </div>
            <div className="flex items-center gap-2">
              <ConnectionTestEnableControls
                connection={connection}
                testDisabled={!setup.data}
                onResult={onControlResult}
              />
            </div>
          </div>
          {testResult && (
            <div
              className={`flex items-start gap-2 border-t px-5 py-3 text-sm ${
                testResult.ok
                  ? "bg-instrument-nominal/[0.07]"
                  : "bg-destructive/[0.06] text-destructive"
              }`}
            >
              {testResult.ok ? (
                <ShieldCheckIcon className="mt-0.5 size-4 shrink-0 text-instrument-nominal" />
              ) : (
                <TriangleAlertIcon className="mt-0.5 size-4 shrink-0" />
              )}
              <span>{testResult.message}</span>
            </div>
          )}
        </section>
      </div>

      <GoogleCloudEndpointDialog
        connection={connection}
        open={editingEndpoints}
        onOpenChange={setEditingEndpoints}
        onSaved={() => {
          setTestResult(null);
          toast.success("Allowed APIs updated; test the connection before enabling it");
        }}
      />
    </main>
  );
}

function BackToGoogleCloud() {
  return (
    <Button asChild variant="ghost" size="sm" className="-ml-2 w-fit text-muted-foreground">
      <Link to="/settings/integrations/$provider" params={{ provider: "gcp" }}>
        <ChevronLeftIcon className="size-4" /> Google Cloud
      </Link>
    </Button>
  );
}

function ProgressStep({
  complete = false,
  active = false,
  title,
  detail,
}: {
  complete?: boolean;
  active?: boolean;
  title: string;
  detail: string;
}) {
  return (
    <li className="flex items-start gap-3">
      {complete ? (
        <CheckCircle2Icon className="mt-0.5 size-4 shrink-0 text-instrument-nominal" />
      ) : (
        <CircleIcon
          className={`mt-0.5 size-4 shrink-0 ${active ? "text-primary" : "text-muted-foreground/50"}`}
        />
      )}
      <div>
        <p className={`text-sm font-medium ${active ? "text-foreground" : ""}`}>{title}</p>
        <p className="mt-0.5 text-xs text-muted-foreground">{detail}</p>
      </div>
    </li>
  );
}

function SetupCode({
  label,
  filename,
  language,
  value,
}: {
  label: string;
  filename: string;
  language: SetupCodeLanguage;
  value: string;
}) {
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value);
    } catch (error) {
      toast.error(errorMessage(error));
      return;
    }
    toast.success(`${label} copied`);
  };

  const download = () => {
    const url = URL.createObjectURL(new Blob([value], { type: "text/plain;charset=utf-8" }));
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = filename;
    anchor.click();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="min-w-0">
      <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
        <div>
          <p className="text-sm font-medium">{label}</p>
          <p className="mt-0.5 text-xs text-muted-foreground">
            Review the configuration in your infrastructure repository before you apply it.
          </p>
        </div>
        <div className="flex items-center gap-1">
          <Button variant="ghost" size="sm" onClick={() => void copy()}>
            <CopyIcon className="size-3.5" /> Copy
          </Button>
          <Button variant="ghost" size="sm" onClick={download}>
            <DownloadIcon className="size-3.5" /> Download
          </Button>
        </div>
      </div>
      <pre className="max-h-[min(62vh,44rem)] min-h-72 w-full overflow-auto rounded-md border bg-muted/30 p-4 font-mono text-xs leading-6 whitespace-pre-wrap break-words">
        <Suspense
          fallback={
            <code data-language={language} data-highlighted="false">
              {value}
            </code>
          }
        >
          <SyntaxHighlightedCode language={language} value={value} />
        </Suspense>
      </pre>
    </div>
  );
}

function Detail({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <p className="text-muted-foreground">{label}</p>
      <p className="mt-1 overflow-x-auto font-mono leading-5 whitespace-nowrap">{value}</p>
    </div>
  );
}
