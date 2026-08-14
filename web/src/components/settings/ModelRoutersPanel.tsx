import { useState } from "react";
import { ExternalLink, KeyRound, RefreshCw, Route, Search } from "lucide-react";

import { PageHeading } from "../page-heading";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { usePutOrgSecret } from "@/hooks/useOrgSecrets";
import {
  useModelRouters,
  useInvalidateRouter,
  useRefreshRouterModels,
  useRouterModels,
  useUpdateRouterModelPolicy,
} from "@/hooks/useModelRouters";
import type { ModelRouter, RouterModel } from "@/gen/engram/app/v1/model_router_pb";

export function ModelRoutersPanel() {
  const { data, isLoading, error } = useModelRouters();
  const routers = data?.routers ?? [];
  return (
    <div className="space-y-6">
      <PageHeading title="Model routers" count={routers.length || undefined} />
      <p className="max-w-3xl text-sm text-muted-foreground">
        Routers supply models independently of the agent harness. A harness can use a routed model
        when both sides share a protocol.
      </p>
      {error && (
        <p className="text-sm text-destructive">Could not load model routers — {String(error)}</p>
      )}
      {isLoading ? (
        <p className="py-8 text-sm text-muted-foreground">Loading…</p>
      ) : (
        routers.map((router) => <RouterCard key={router.id} router={router} />)
      )}
    </div>
  );
}

function RouterCard({ router }: { router: ModelRouter }) {
  const [search, setSearch] = useState("");
  const { data, isLoading, error } = useRouterModels(router.id, search);
  const refresh = useRefreshRouterModels(router.id, search);
  const update = useUpdateRouterModelPolicy(router.id, search);
  const putSecret = usePutOrgSecret();
  const invalidate = useInvalidateRouter(router.id, search);
  const [secret, setSecret] = useState("");
  const models = data?.models ?? [];

  const saveSecret = async () => {
    if (!secret) return;
    await putSecret.mutateAsync({ name: router.credentialSecret, value: secret });
    await invalidate();
    setSecret("");
  };

  return (
    <Card className="overflow-hidden">
      <CardContent className="p-0">
        <div className="border-b bg-[linear-gradient(110deg,var(--muted),transparent_72%)] p-5">
          <div className="flex flex-wrap items-start justify-between gap-4">
            <div>
              <div className="flex items-center gap-2">
                <Route className="size-4" aria-hidden="true" />
                <h2 className="text-base font-semibold">{router.label}</h2>
                <Badge variant={router.credentialConfigured ? "secondary" : "destructive"}>
                  {router.credentialConfigured ? "connected" : "key required"}
                </Badge>
              </div>
              <p className="mt-1 text-sm text-muted-foreground">{router.description}</p>
            </div>
            <Button
              variant="outline"
              size="sm"
              disabled={refresh.isPending || !router.credentialConfigured}
              onClick={() => refresh.mutate({ routerId: router.id })}
            >
              <RefreshCw className={refresh.isPending ? "animate-spin" : ""} />
              Refresh catalog
            </Button>
          </div>

          <div className="mt-4 grid gap-3 lg:grid-cols-[minmax(0,1fr)_auto]">
            <div className="flex items-center gap-2 rounded-md border bg-background/80 p-2">
              <KeyRound className="ml-1 size-4 text-muted-foreground" />
              <Input
                type="password"
                value={secret}
                onChange={(event) => setSecret(event.target.value)}
                placeholder={
                  router.credentialConfigured
                    ? "Paste a replacement key"
                    : `Set ${router.credentialSecret}`
                }
                aria-label={
                  router.credentialConfigured ? "Replacement OpenRouter key" : "OpenRouter key"
                }
                className="border-0 bg-transparent font-mono shadow-none focus-visible:ring-0"
              />
              <Button
                size="sm"
                disabled={!secret || putSecret.isPending}
                onClick={() => void saveSecret()}
              >
                {router.credentialConfigured ? "Replace key" : "Save key"}
              </Button>
            </div>
            <div className="flex items-center gap-1 rounded-md border bg-background/80 px-3 py-2 text-xs">
              {router.protocols.map((protocol, index) => (
                <span key={protocol} className="contents">
                  {index > 0 && <span className="text-muted-foreground">+</span>}
                  <Badge variant="outline" className="font-mono font-normal">
                    {protocol}
                  </Badge>
                </span>
              ))}
              <span className="mx-1 text-muted-foreground">→</span>
              <span className="font-mono">openrouter.ai</span>
            </div>
          </div>

          <div className="mt-3 flex flex-wrap gap-x-5 gap-y-1 text-xs text-muted-foreground">
            <span>{router.availableModelCount.toLocaleString()} available</span>
            <span>{router.enabledModelCount.toLocaleString()} enabled</span>
            <span>{router.modelCount.toLocaleString()} cached</span>
            <span>
              Last refresh:{" "}
              {router.lastSuccessfulSyncAt
                ? new Date(router.lastSuccessfulSyncAt).toLocaleString()
                : "never"}
            </span>
          </div>
          {router.lastSyncError && (
            <p className="mt-2 text-xs text-destructive">
              Last refresh failed: {router.lastSyncError}
            </p>
          )}
          {(putSecret.error || refresh.error) && (
            <p className="mt-2 text-xs text-destructive">
              {String(putSecret.error ?? refresh.error)}
            </p>
          )}
        </div>

        <div className="p-4">
          <div className="relative mb-3 max-w-lg">
            <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder="Search model, author, or slug"
              className="pl-9"
            />
          </div>
          {error ? (
            <p className="text-sm text-destructive">Could not load models — {String(error)}</p>
          ) : isLoading ? (
            <p className="py-8 text-sm text-muted-foreground">Loading catalog…</p>
          ) : (
            <ModelTable
              models={models}
              busy={update.isPending}
              onPolicy={(model, enabled, userEnabled) =>
                update.mutate({ routerId: router.id, modelId: model.id, enabled, userEnabled })
              }
            />
          )}
        </div>
      </CardContent>
    </Card>
  );
}

function ModelTable({
  models,
  busy,
  onPolicy,
}: {
  models: RouterModel[];
  busy: boolean;
  onPolicy: (model: RouterModel, enabled: boolean, userEnabled: boolean) => void;
}) {
  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>Model</TableHead>
          <TableHead>Capabilities</TableHead>
          <TableHead>Context</TableHead>
          <TableHead>Price / 1M</TableHead>
          <TableHead>Enabled</TableHead>
          <TableHead>Available to users</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {models.map((model) => (
          <TableRow key={model.id} className={!model.available ? "opacity-55" : undefined}>
            <TableCell className="min-w-72 whitespace-normal">
              <div className="flex items-center gap-2">
                <span className="font-medium">{model.name}</span>
                {!model.available && <Badge variant="outline">unavailable</Badge>}
              </div>
              <div className="mt-0.5 flex items-center gap-2 text-xs text-muted-foreground">
                {model.author && <span>{model.author}</span>}
                <span className="font-mono">{model.id}</span>
                <a
                  href={model.upstreamUrl}
                  target="_blank"
                  rel="noreferrer"
                  aria-label={`Open ${model.name} on OpenRouter`}
                >
                  <ExternalLink className="size-3" />
                </a>
                {model.huggingFaceId && (
                  <a
                    className="underline-offset-2 hover:underline"
                    href={`https://huggingface.co/${model.huggingFaceId}`}
                    target="_blank"
                    rel="noreferrer"
                  >
                    HF
                  </a>
                )}
              </div>
            </TableCell>
            <TableCell>
              <div className="flex max-w-64 flex-wrap gap-1">
                <Badge variant="secondary">tools</Badge>
                {model.supportsReasoning && <Badge variant="outline">reasoning</Badge>}
                {model.inputModalities
                  .filter((item) => item !== "text")
                  .map((item) => (
                    <Badge key={item} variant="outline">
                      {item}
                    </Badge>
                  ))}
              </div>
            </TableCell>
            <TableCell>{formatContext(model.contextLength)}</TableCell>
            <TableCell className="font-mono text-xs">
              {formatPrice(model.promptPrice)} / {formatPrice(model.completionPrice)}
            </TableCell>
            <TableCell>
              <Switch
                checked={model.enabled}
                disabled={busy}
                aria-label={`Enable ${model.name}`}
                onCheckedChange={(enabled) =>
                  onPolicy(model, enabled, enabled ? model.userEnabled : false)
                }
              />
            </TableCell>
            <TableCell>
              <Switch
                checked={model.userEnabled}
                disabled={busy || !model.enabled}
                aria-label={`Make ${model.name} available to users`}
                onCheckedChange={(userEnabled) => onPolicy(model, true, userEnabled)}
              />
            </TableCell>
          </TableRow>
        ))}
        {models.length === 0 && (
          <TableRow>
            <TableCell colSpan={6} className="h-28 text-center text-muted-foreground">
              No models match this search.
            </TableCell>
          </TableRow>
        )}
      </TableBody>
    </Table>
  );
}

function formatContext(value: bigint): string {
  const number = Number(value);
  return number >= 1_000_000
    ? `${(number / 1_000_000).toFixed(1)}M`
    : `${Math.round(number / 1_000)}K`;
}

function formatPrice(value?: string): string {
  if (!value) return "—";
  const perMillion = Number(value) * 1_000_000;
  return Number.isFinite(perMillion) ? `$${perMillion.toFixed(perMillion < 1 ? 2 : 1)}` : "—";
}
