/**
 * ConnectSheet — the guided "connect a provider" flow (right-side Sheet).
 *
 * Two steps, Authenticate → Review. (A "Test connection" step lands with the
 * coordinator TestConnector RPC; it slots between these without reshaping them.)
 *  - mint   → collect the mint kind's fields → SetMintCredential (seals
 *             `<kind>.<field>` org secrets).
 *  - inject → collect one secret → PutOrgSecret(secretRef).
 * Review shows the powers this unlocks + the egress hosts it opens.
 */

import { useState } from "react";
import { toast } from "sonner";
import { ArrowRightIcon, CheckIcon, InfoIcon, LockIcon, ShieldCheckIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Sheet, SheetContent } from "@/components/ui/sheet";
import { Text } from "@/components/ui/text";
import { MintFieldKind } from "@/gen/engram/app/v1/mint_pb";
import { useConnectors, useMintKinds, useSetMintCredential } from "@/hooks/useIntegrations";
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

  const isMint = view.credentialSource === "mint";
  const row = conns.data?.connectors.find((c) => c.provider === view.provider);
  const cfg = row ? parseConnectorConfig(row.configJson, view.provider) : undefined;
  const mintKind = isMint
    ? mintKinds.data?.mintKinds.find(
        (k) => k.provider === view.provider || k.kind === cfg?.mintKind,
      )
    : undefined;

  const [step, setStep] = useState(0);
  const [values, setValues] = useState<Record<string, string>>({});
  const [injectSecret, setInjectSecret] = useState("");
  const [error, setError] = useState<string | null>(null);

  const filled = isMint
    ? (mintKind?.fields ?? []).filter((f) => f.required).every((f) => (values[f.name] ?? "").trim())
    : injectSecret.trim().length > 0;
  const pending = setMint.isPending || putSecret.isPending;

  const finish = async () => {
    setError(null);
    try {
      if (isMint) {
        if (!mintKind) throw new Error("no mint kind for this provider");
        await setMint.mutateAsync({ provider: view.provider, kind: mintKind.kind, values });
      } else {
        if (!cfg?.secretRef) throw new Error("connector has no secret ref");
        await putSecret.mutateAsync({ name: cfg.secretRef, value: injectSecret });
      }
      toast.success(`${view.name} connected — ${view.capabilities.length} powers now grantable`);
      onConnected(view.provider);
    } catch (e) {
      setError(String(e));
    }
  };

  const steps = ["Authenticate", "Review"];

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
          {step === 0 ? (
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
                <label className="flex flex-col gap-1.5">
                  <Text variant="label">{view.name} credential</Text>
                  <SecretField
                    value={injectSecret}
                    onChange={setInjectSecret}
                    placeholder="••••••"
                  />
                </label>
              )}

              <p className="flex gap-2 text-[0.74rem] text-muted-foreground">
                <InfoIcon className="mt-0.5 size-3.5 shrink-0" />
                {isMint ? (
                  "Stored as org secrets; the platform exchanges them for a short-lived token at session start."
                ) : (
                  <span>
                    Sealed in the org secret store as{" "}
                    <code className="font-mono">{cfg?.secretRef}</code>.
                  </span>
                )}
              </p>
            </div>
          ) : (
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
            <Button onClick={() => setStep(step + 1)} disabled={!filled}>
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
