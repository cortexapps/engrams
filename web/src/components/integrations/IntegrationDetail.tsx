/**
 * IntegrationDetail — per-provider manage page (/settings/integrations/$provider).
 * Credential posture + Replace, the powers it grants, the egress it opens, a
 * sample of how it appears in a session, the profiles that use it, and (custom
 * only) a remove control. A "Test" affordance lands with the coordinator
 * TestConnector RPC.
 */

import { useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  ArrowUpRightIcon,
  ChevronLeftIcon,
  CircleCheckIcon,
  ExternalLinkIcon,
  LayersIcon,
  Loader2Icon,
  LockIcon,
  RotateCcwIcon,
  ShieldCheckIcon,
  Trash2Icon,
  TriangleAlertIcon,
  ZapIcon,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { MintFieldKind } from "@/gen/engram/app/v1/mint_pb";
import {
  useConnectors,
  useDeleteConnector,
  useMintKinds,
  useTestConnector,
} from "@/hooks/useIntegrations";
import { humanizeAction, parseConnectorConfig } from "@/lib/connectorModel";
import { ProviderTile } from "./ProviderTile";
import { AccessTag, HostChip, StatusDot } from "./chips";
import { ReplaceCredentialSheet } from "./ReplaceCredentialSheet";
import { useConnectorViews, type ConnectorView } from "./useConnectorViews";

export function IntegrationDetail() {
  const { provider } = useParams({ strict: false }) as { provider?: string };
  const { views, isLoading } = useConnectorViews();
  const view = views.find((v) => v.provider === provider);

  // Surface the OAuth callback outcome (the "Add to {provider}" redirect lands
  // back here with ?connected=1 / ?error=…), then strip the one-shot flag.
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    if (params.get("connected") === "1") {
      toast.success("Connected — workspace token stored");
    } else if (params.get("error")) {
      toast.error(`Connection failed: ${params.get("error")}`);
    } else {
      return;
    }
    window.history.replaceState({}, "", window.location.pathname);
  }, []);

  if (isLoading) return <p className="py-6 text-sm text-muted-foreground">Loading…</p>;
  if (!view) {
    return (
      <div className="mx-auto max-w-3xl">
        <BackLink />
        <p className="mt-4 text-sm text-muted-foreground">No connector "{provider}".</p>
      </div>
    );
  }
  return <DetailBody view={view} />;
}

function BackLink() {
  return (
    <Button asChild variant="ghost" size="sm" className="-ml-2 text-muted-foreground">
      <Link to="/settings/integrations">
        <ChevronLeftIcon className="size-4" />
        Integrations
      </Link>
    </Button>
  );
}

function DetailBody({ view }: { view: ConnectorView }) {
  const navigate = useNavigate();
  const conns = useConnectors();
  const mintKinds = useMintKinds();
  const del = useDeleteConnector();
  const test = useTestConnector();
  const [replacing, setReplacing] = useState(false);
  const [confirmRemove, setConfirmRemove] = useState(false);
  const [testState, setTestState] = useState<"idle" | "ok" | "fail">("idle");
  const [testMessage, setTestMessage] = useState("");

  const runTest = async () => {
    setTestState("idle");
    try {
      // Empty draft → the coordinator tests the stored credential.
      const r = await test.mutateAsync({ provider: view.provider, draftValues: {} });
      setTestState(r.ok ? "ok" : "fail");
      setTestMessage(r.message);
    } catch (e) {
      setTestState("fail");
      setTestMessage(e instanceof Error ? e.message : String(e));
    }
  };

  const row = conns.data?.connectors.find((c) => c.provider === view.provider);
  const cfg = row ? parseConnectorConfig(row.configJson, view.provider) : undefined;
  const isMint = view.credentialSource === "mint";
  const mintKind = isMint
    ? mintKinds.data?.mintKinds.find(
        (k) => k.provider === view.provider || k.kind === cfg?.mintKind,
      )
    : undefined;

  const samples = sampleEvents(view);

  const remove = () => {
    del.mutate(
      { provider: view.provider },
      {
        onSuccess: () => {
          toast.success(`${view.name} removed`);
          void navigate({ to: "/settings/integrations" });
        },
      },
    );
  };

  return (
    <div className="mx-auto flex max-w-3xl flex-col gap-6">
      <div>
        <BackLink />
        <div className="mt-3 flex items-start gap-4">
          <ProviderTile {...view.icon} name={view.name} size={52} />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2.5">
              <h1 className="font-display text-2xl font-semibold [font-stretch:108%]">
                {view.name}
              </h1>
              {view.status === "connected" && <StatusDot tone="nominal" label="connected" />}
            </div>
            <div className="mt-1 flex items-center gap-2.5 text-sm text-muted-foreground">
              <Text variant="label" tone="muted" className="text-[0.56rem]">
                {view.category}
              </Text>
              <span>· {view.builtin ? "built-in" : "custom"}</span>
            </div>
          </div>
          <Button variant="outline" size="sm" onClick={runTest} disabled={test.isPending}>
            {test.isPending ? (
              <>
                <Loader2Icon className="size-3.5 animate-spin" />
                Testing…
              </>
            ) : (
              <>
                <ZapIcon className="size-3.5" />
                Test
              </>
            )}
          </Button>
        </div>
        {testState !== "idle" && (
          <div
            className={`mt-2.5 flex items-center gap-2 rounded-md border px-3 py-2 text-xs ${
              testState === "ok"
                ? "border-instrument-nominal/45 bg-instrument-nominal/[0.09]"
                : "border-destructive/45 bg-destructive/[0.06]"
            }`}
          >
            {testState === "ok" ? (
              <CircleCheckIcon className="size-3.5 shrink-0 text-instrument-nominal" />
            ) : (
              <TriangleAlertIcon className="size-3.5 shrink-0 text-destructive" />
            )}
            <span className="font-mono text-muted-foreground">{testMessage}</span>
          </div>
        )}
      </div>

      {/* credential */}
      <div className="rounded-lg border bg-card p-4">
        <div className="flex items-start gap-3">
          {isMint ? (
            <ShieldCheckIcon className="mt-0.5 size-4 text-instrument-nominal" />
          ) : (
            <LockIcon className="mt-0.5 size-4 text-instrument-nominal" />
          )}
          <div className="flex-1">
            <div className="text-sm font-semibold">
              {isMint ? "Minted per session" : "Brokered credential"}
            </div>
            {isMint ? (
              <div className="mt-2 flex flex-wrap gap-1.5">
                {(mintKind?.fields ?? []).map((f) => (
                  <span
                    key={f.name}
                    className="inline-flex items-center gap-1.5 rounded-sm border bg-secondary px-2 py-0.5 font-mono text-xs"
                  >
                    <span className="text-muted-foreground">{f.label}</span>
                    <span>
                      {f.fieldKind === MintFieldKind.SEALED_SECRET
                        ? "•••• set"
                        : view.status === "connected"
                          ? "set"
                          : "—"}
                    </span>
                  </span>
                ))}
              </div>
            ) : (
              <div className="mt-1.5 flex flex-col gap-0.5 font-mono text-xs text-muted-foreground">
                {(cfg?.injects ?? []).map((inj) => (
                  <div key={inj.secretRef}>
                    org secret · {inj.secretRef} · header {inj.header}: {inj.template}
                  </div>
                ))}
              </div>
            )}
          </div>
          <Button variant="ghost" size="sm" onClick={() => setReplacing(true)}>
            <RotateCcwIcon className="size-3.5" />
            Replace
          </Button>
        </div>
      </div>

      {/* powers */}
      <section className="flex flex-col gap-2.5">
        <Text variant="label">Powers · {view.capabilities.length}</Text>
        <div className="flex flex-col gap-1.5">
          {view.capabilities.map((cap) => (
            <div
              key={cap.action}
              className="flex items-center gap-2.5 rounded-md border bg-card px-3 py-2"
            >
              <span className="flex-1 text-sm">{humanizeAction(cap.action)}</span>
              {cap.asset && (
                <span className="inline-flex items-center gap-1 text-[0.7rem] text-muted-foreground">
                  <LayersIcon className="size-3" />
                  {cap.asset.replace(/_/g, " ")}
                </span>
              )}
              <code className="font-mono text-xs text-muted-foreground">
                {view.provider}:{cap.action}
              </code>
              <AccessTag access={cap.access} />
            </div>
          ))}
        </div>
      </section>

      {/* egress */}
      <section className="flex flex-col gap-2.5">
        <Text variant="label">Egress it can open</Text>
        <div className="flex flex-wrap gap-1.5">
          {view.hosts.map((h) => (
            <HostChip key={h} host={h} derived />
          ))}
        </div>
      </section>

      {/* in a session */}
      <section className="flex flex-col gap-2">
        <Text variant="label">In a session</Text>
        <p className="text-[0.78rem] leading-relaxed text-muted-foreground">
          Every call this integration makes is tagged with its mark — in the run timeline, in audit,
          and on any asset it produces.
        </p>
        <div className="overflow-hidden rounded-lg border bg-card">
          {samples.map((e, i) => (
            <div
              key={i}
              className={`flex items-center gap-2.5 px-3 py-2.5 ${i > 0 ? "border-t" : ""}`}
            >
              <ProviderTile {...view.icon} name={view.name} size={20} />
              <span className="text-sm">{e.verb}</span>
              {e.ref && <code className="font-mono text-xs text-muted-foreground">{e.ref}</code>}
              <span className="flex-1" />
              {e.asset && (
                <span className="inline-flex items-center gap-1 text-[0.68rem] text-muted-foreground">
                  <LayersIcon className="size-3" />
                  {e.asset.replace(/_/g, " ")}
                </span>
              )}
              {e.external && (
                <span className="inline-flex items-center gap-1 text-xs text-primary">
                  <ExternalLinkIcon className="size-3" />
                  open
                </span>
              )}
              <AccessTag access={e.access} />
            </div>
          ))}
        </div>
      </section>

      {/* used by */}
      <section className="flex flex-col gap-2.5">
        <Text variant="label">
          Used by · {view.usedBy} profile{view.usedBy === 1 ? "" : "s"}
        </Text>
        {view.usedByProfiles.length === 0 ? (
          <span className="text-sm text-muted-foreground">No profile grants its powers yet.</span>
        ) : (
          <div className="flex flex-wrap gap-2">
            {view.usedByProfiles.map((p) => (
              <Button key={p.id} asChild variant="secondary" size="sm" className="rounded-full">
                <Link to="/settings/profiles/$id" params={{ id: p.id }}>
                  {p.name}
                  <ArrowUpRightIcon className="size-3 opacity-60" />
                </Link>
              </Button>
            ))}
          </div>
        )}
      </section>

      {/* danger zone (custom only) */}
      {!view.builtin && (
        <div className="border-t pt-4">
          {confirmRemove ? (
            <div className="flex items-center gap-3 rounded-lg border border-destructive/40 bg-destructive/5 p-3">
              <TriangleAlertIcon className="size-4 text-destructive" />
              <span className="flex-1 text-sm">
                {view.usedBy > 0
                  ? `${view.usedBy} profile(s) grant ${view.provider} powers — they'll stop unlocking on the next session.`
                  : "Remove this connector? The stored credential is kept."}
              </span>
              <Button variant="ghost" size="sm" onClick={() => setConfirmRemove(false)}>
                Cancel
              </Button>
              <Button variant="destructive" size="sm" onClick={remove} disabled={del.isPending}>
                Remove
              </Button>
            </div>
          ) : (
            <Button
              variant="ghost"
              size="sm"
              className="text-destructive"
              onClick={() => setConfirmRemove(true)}
            >
              <Trash2Icon className="size-3.5" />
              Remove integration
            </Button>
          )}
        </div>
      )}

      {replacing && (
        <ReplaceCredentialSheet
          view={view}
          onClose={() => setReplacing(false)}
          onReplaced={() => setReplacing(false)}
        />
      )}
    </div>
  );
}

interface SampleEvent {
  verb: string;
  ref?: string;
  asset?: string;
  external?: boolean;
  access: "read" | "write";
}

/** Representative in-session events derived from the connector's powers. */
function sampleEvents(view: ConnectorView): SampleEvent[] {
  const out: SampleEvent[] = [];
  const writeAssets = view.capabilities.filter((c) => c.access === "write" && c.asset).slice(0, 2);
  writeAssets.forEach((c, i) =>
    out.push({
      verb: humanizeAction(c.action),
      ref: `#${1481 - i * 36}`,
      asset: c.asset,
      external: true,
      access: "write",
    }),
  );
  if (out.length < 3) {
    view.capabilities
      .filter((c) => c.access === "read")
      .slice(0, 3 - out.length)
      .forEach((c) => out.push({ verb: humanizeAction(c.action), access: "read" }));
  }
  if (out.length === 0 && view.capabilities[0]) {
    out.push({
      verb: humanizeAction(view.capabilities[0].action),
      access: view.capabilities[0].access,
    });
  }
  return out;
}
