/**
 * ConnectSheet — the guided "connect a provider" flow (right-side Sheet).
 *
 * Three steps, Authenticate → Test → Review:
 *  - mint   → collect the mint kind's fields → SetMintCredential (seals
 *             `<kind>.<field>` org secrets).
 *  - inject → collect one secret → PutOrgSecret(secretRef).
 * Test makes one real authenticated request (the just-entered draft) and gates
 * Continue; Review shows the powers this unlocks + the egress hosts it opens.
 */

import { useState } from "react";
import { toast } from "sonner";
import {
  ArrowRightIcon,
  CheckIcon,
  CircleCheckIcon,
  InfoIcon,
  Loader2Icon,
  LockIcon,
  ShieldCheckIcon,
  TriangleAlertIcon,
  ZapIcon,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Sheet, SheetContent } from "@/components/ui/sheet";
import { Text } from "@/components/ui/text";
import { MintFieldKind } from "@/gen/engram/app/v1/mint_pb";
import {
  useConnectors,
  useMintKinds,
  useSetMintCredential,
  useTestConnector,
} from "@/hooks/useIntegrations";
import { usePutOrgSecret } from "@/hooks/useOrgSecrets";
import { humanizeAction, parseConnectorConfig } from "@/lib/connectorModel";
import { ProviderTile } from "./ProviderTile";
import { AccessTag, HostChip } from "./chips";
import { SecretField } from "./SecretField";
import type { ConnectorView } from "./useConnectorViews";

export function ConnectSheet({
  view,
  onClose,
  onConnected,
}: {
  view: ConnectorView;
  onClose: () => void;
  onConnected: (provider: string) => void;
}) {
  const conns = useConnectors();
  const mintKinds = useMintKinds();
  const setMint = useSetMintCredential();
  const putSecret = usePutOrgSecret();
  const test = useTestConnector();

  const isMint = view.credentialSource === "mint";
  const row = conns.data?.connectors.find((c) => c.provider === view.provider);
  const cfg = row ? parseConnectorConfig(row.configJson, view.provider) : undefined;
  const mintKind = isMint
    ? mintKinds.data?.mintKinds.find(
        (k) => k.provider === view.provider || k.kind === cfg?.mintKind,
      )
    : undefined;
  const injects = cfg?.injects ?? [];

  const [step, setStep] = useState(0);
  const [values, setValues] = useState<Record<string, string>>({});
  // ADR 0058: one entry per injected header, keyed by its org-secret ref.
  const [injectSecrets, setInjectSecrets] = useState<Record<string, string>>({});
  const [testState, setTestState] = useState<"idle" | "ok" | "fail">("idle");
  const [testMessage, setTestMessage] = useState("");
  const [error, setError] = useState<string | null>(null);

  const filled = isMint
    ? (mintKind?.fields ?? []).filter((f) => f.required).every((f) => (values[f.name] ?? "").trim())
    : injects.length > 0 && injects.every((i) => (injectSecrets[i.secretRef] ?? "").trim().length > 0);
  const pending = setMint.isPending || putSecret.isPending;

  const runTest = async () => {
    setTestState("idle");
    // ADR 0058: send every entered secret keyed by its org-secret ref, so the
    // test probes ALL the connector's headers (the orchestrator maps each by ref).
    const draftValues = isMint ? values : injectSecrets;
    try {
      const r = await test.mutateAsync({ provider: view.provider, draftValues });
      setTestState(r.ok ? "ok" : "fail");
      setTestMessage(r.message);
    } catch (e) {
      setTestState("fail");
      setTestMessage(e instanceof Error ? e.message : String(e));
    }
  };

  const finish = async () => {
    setError(null);
    try {
      if (isMint) {
        if (!mintKind) throw new Error("no mint kind for this provider");
        await setMint.mutateAsync({ provider: view.provider, kind: mintKind.kind, values });
      } else {
        if (injects.length === 0) throw new Error("connector has no inject credential");
        for (const inj of injects) {
          await putSecret.mutateAsync({ name: inj.secretRef, value: injectSecrets[inj.secretRef] ?? "" });
        }
      }
      toast.success(`${view.name} connected — ${view.capabilities.length} powers now grantable`);
      onConnected(view.provider);
    } catch (e) {
      setError(String(e));
    }
  };

  const steps = ["Authenticate", "Test", "Review"];

  return (
    <Sheet open onOpenChange={(o) => !o && onClose()}>
      <SheetContent side="right" className="flex w-full flex-col gap-0 p-0 sm:max-w-[580px]">
        <header className="flex items-center gap-3 border-b p-5">
          <ProviderTile {...view.icon} name={view.name} size={40} />
          <div className="min-w-0 flex-1">
            <div className="font-display text-[1.05rem] font-semibold">Connect {view.name}</div>
            <div className="text-xs text-muted-foreground">{view.category}</div>
          </div>
        </header>

        <div className="flex gap-2 border-b px-5 py-3">
          {steps.map((s, i) => (
            <div key={s} className="flex flex-1 items-center gap-2">
              <span
                className={`inline-flex size-5 shrink-0 items-center justify-center rounded-full border font-mono text-[0.66rem] font-bold ${
                  i < step
                    ? "border-primary bg-primary text-primary-foreground"
                    : i === step
                      ? "border-primary bg-primary/20"
                      : "border-border bg-secondary"
                }`}
              >
                {i < step ? <CheckIcon className="size-3" /> : i + 1}
              </span>
              <Text
                variant="label"
                tone={i === step ? "default" : "muted"}
                className="text-[0.62rem]"
              >
                {s}
              </Text>
            </div>
          ))}
        </div>

        <div className="flex-1 overflow-y-auto p-5">
          {step === 0 && (
            <div className="flex flex-col gap-4">
              <div className="flex gap-3 rounded-lg border bg-secondary p-3">
                {isMint ? (
                  <ShieldCheckIcon className="mt-0.5 size-4 shrink-0 text-instrument-nominal" />
                ) : (
                  <LockIcon className="mt-0.5 size-4 shrink-0 text-instrument-nominal" />
                )}
                <div>
                  <div className="text-sm font-semibold">
                    {isMint
                      ? "Engram mints credentials per session"
                      : "Brokered credential — never in the sandbox"}
                  </div>
                  <div className="mt-0.5 text-[0.78rem] leading-relaxed text-muted-foreground">
                    {isMint
                      ? "Your credentials are exchanged for a short-lived, scoped token at session start — nothing is stored in the sandbox."
                      : "Stored once as an org secret, brokered into the request at the egress proxy — the value never enters the sandbox."}
                  </div>
                </div>
              </div>

              {isMint ? (
                (mintKind?.fields ?? []).map((f) => (
                  <label key={f.name} className="flex flex-col gap-1.5">
                    <span className="flex items-baseline gap-2">
                      <Text variant="label">{f.label}</Text>
                      {f.required && (
                        <span className="font-display text-[0.6rem] tracking-[0.08em] text-instrument-caution">
                          REQUIRED
                        </span>
                      )}
                    </span>
                    {f.fieldKind === MintFieldKind.SEALED_SECRET ? (
                      <SecretField
                        value={values[f.name] ?? ""}
                        onChange={(v) => setValues((s) => ({ ...s, [f.name]: v }))}
                      />
                    ) : (
                      <Input
                        className="font-mono"
                        autoComplete="off"
                        spellCheck={false}
                        value={values[f.name] ?? ""}
                        onChange={(e) => setValues((s) => ({ ...s, [f.name]: e.target.value }))}
                      />
                    )}
                  </label>
                ))
              ) : (
                injects.map((inj) => (
                  <label key={inj.secretRef} className="flex flex-col gap-1.5">
                    <span className="flex items-baseline gap-2">
                      <Text variant="label">{inj.header}</Text>
                      <code className="font-mono text-[0.62rem] text-muted-foreground">
                        {inj.secretRef}
                      </code>
                    </span>
                    <SecretField
                      value={injectSecrets[inj.secretRef] ?? ""}
                      onChange={(v) => setInjectSecrets((s) => ({ ...s, [inj.secretRef]: v }))}
                      placeholder="••••••"
                    />
                  </label>
                ))
              )}

              <p className="flex gap-2 text-[0.74rem] text-muted-foreground">
                <InfoIcon className="mt-0.5 size-3.5 shrink-0" />
                {isMint ? (
                  "Stored as org secrets; the platform exchanges them for a short-lived token at session start."
                ) : (
                  <span>
                    Sealed in the org secret store as{" "}
                    {injects.map((inj, i) => (
                      <span key={inj.secretRef}>
                        {i > 0 ? ", " : ""}
                        <code className="font-mono">{inj.secretRef}</code>
                      </span>
                    ))}
                    .
                  </span>
                )}
              </p>
            </div>
          )}

          {step === 1 && (
            <div className="flex flex-col gap-4">
              <p className="text-sm leading-relaxed text-muted-foreground">
                We'll make one real request to <code className="font-mono">{view.hosts[0]}</code>{" "}
                with the credential you entered — just an authenticated ping, no scopes exercised.
              </p>
              <div className="flex justify-center py-1">
                <Button onClick={runTest} disabled={test.isPending}>
                  {test.isPending ? (
                    <>
                      <Loader2Icon className="size-4 animate-spin" />
                      Testing…
                    </>
                  ) : (
                    <>
                      <ZapIcon className="size-4" />
                      {testState === "ok" ? "Test again" : "Test connection"}
                    </>
                  )}
                </Button>
              </div>
              {testState === "ok" && (
                <div className="flex gap-3 rounded-lg border border-instrument-nominal/45 bg-instrument-nominal/[0.09] p-3.5">
                  <CircleCheckIcon className="mt-0.5 size-4 shrink-0 text-instrument-nominal" />
                  <div>
                    <div className="text-sm font-semibold">Connection verified</div>
                    <div className="mt-0.5 font-mono text-xs leading-relaxed text-muted-foreground">
                      {testMessage}
                    </div>
                  </div>
                </div>
              )}
              {testState === "fail" && (
                <div className="flex gap-3 rounded-lg border border-destructive/45 bg-destructive/[0.06] p-3.5">
                  <TriangleAlertIcon className="mt-0.5 size-4 shrink-0 text-destructive" />
                  <div className="text-sm">{testMessage}</div>
                </div>
              )}
            </div>
          )}

          {step === 2 && (
            <div className="flex flex-col gap-5">
              <div className="flex flex-col gap-2">
                <Text variant="label">Powers this unlocks</Text>
                {view.capabilities.map((cap) => (
                  <div
                    key={cap.action}
                    className="flex items-center gap-2.5 rounded-md border bg-card px-3 py-1.5"
                  >
                    <span className="flex-1 text-sm">{humanizeAction(cap.action)}</span>
                    <code className="font-mono text-xs text-muted-foreground">
                      {view.provider}:{cap.action}
                    </code>
                    <AccessTag access={cap.access} />
                  </div>
                ))}
              </div>
              <div className="flex flex-col gap-2">
                <Text variant="label">Egress this can open</Text>
                <div className="flex flex-wrap gap-1.5">
                  {view.hosts.map((h) => (
                    <HostChip key={h} host={h} derived />
                  ))}
                </div>
                <p className="text-[0.74rem] text-muted-foreground">
                  Only opened for sessions whose profile grants a power above. Everything else stays
                  denied.
                </p>
              </div>
            </div>
          )}
          {error && <p className="mt-3 text-sm text-destructive">{error}</p>}
        </div>

        <footer className="flex items-center justify-between gap-2 border-t p-5">
          <Button variant="ghost" onClick={step === 0 ? onClose : () => setStep(step - 1)}>
            {step === 0 ? "Cancel" : "Back"}
          </Button>
          {step < steps.length - 1 ? (
            <Button
              onClick={() => {
                if (step === 0) setTestState("idle"); // re-test after any cred edit
                setStep(step + 1);
              }}
              disabled={step === 0 ? !filled : testState !== "ok"}
            >
              Continue
              <ArrowRightIcon className="size-3.5" />
            </Button>
          ) : (
            <Button onClick={finish} disabled={pending || !filled}>
              <CheckIcon className="size-3.5" />
              {pending ? "Connecting…" : `Add ${view.name}`}
            </Button>
          )}
        </footer>
      </SheetContent>
    </Sheet>
  );
}
