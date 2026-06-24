/**
 * ReplaceCredentialSheet — rotate a connected provider's credential (right Sheet).
 * mint → re-seal the kind's fields (blank = keep); inject → overwrite the org
 * secret. The new value is used from the next session on; running sessions keep
 * their current credential. "Test new credential" runs a real authenticated
 * probe with the just-entered draft before sealing.
 */

import { useState } from "react";
import { toast } from "sonner";
import {
  CheckIcon,
  CircleCheckIcon,
  InfoIcon,
  Loader2Icon,
  LockIcon,
  ShieldCheckIcon,
  ZapIcon,
} from "lucide-react";

import { Button } from "@/components/ui/button";
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
import { parseConnectorConfig } from "@/lib/connectorModel";
import { ProviderTile } from "./ProviderTile";
import { SecretField } from "./SecretField";
import type { ConnectorView } from "./useConnectorViews";

export function ReplaceCredentialSheet({
  view,
  onClose,
  onReplaced,
}: {
  view: ConnectorView;
  onClose: () => void;
  onReplaced: () => void;
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

  const [values, setValues] = useState<Record<string, string>>({});
  // ADR 0058: one entry per injected header, keyed by its org-secret ref.
  const [injectSecrets, setInjectSecrets] = useState<Record<string, string>>({});
  const [testState, setTestState] = useState<"idle" | "ok" | "fail">("idle");
  const [testMessage, setTestMessage] = useState("");
  const [error, setError] = useState<string | null>(null);

  const canSave = isMint
    ? Object.values(values).some((v) => v.trim().length > 0)
    : Object.values(injectSecrets).some((v) => v.trim().length > 0);
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

  const save = async () => {
    setError(null);
    try {
      if (isMint) {
        if (!mintKind) throw new Error("no mint kind for this provider");
        // Blank fields are skipped server-side (leave existing unchanged).
        await setMint.mutateAsync({ provider: view.provider, kind: mintKind.kind, values });
      } else {
        // Blank fields are skipped — leave that secret unchanged.
        for (const inj of injects) {
          const v = injectSecrets[inj.secretRef];
          if (v && v.trim().length > 0) await putSecret.mutateAsync({ name: inj.secretRef, value: v });
        }
      }
      toast.success(`${view.name} credential replaced — sealed in the org secret store`);
      onReplaced();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <Sheet open onOpenChange={(o) => !o && onClose()}>
      <SheetContent side="right" className="flex w-full flex-col gap-0 p-0 sm:max-w-[560px]">
        <header className="flex items-center gap-3 border-b p-5">
          <ProviderTile {...view.icon} name={view.name} size={38} />
          <div className="min-w-0 flex-1">
            <div className="font-display text-[1.02rem] font-semibold">
              Replace {view.name} credential
            </div>
            <div className="text-xs text-muted-foreground">
              {isMint
                ? "Rotate the minted credentials"
                : `Rotate the org secret${injects.length > 1 ? "s" : ""} · ${injects.map((i) => i.secretRef).join(", ")}`}
            </div>
          </div>
        </header>

        <div className="flex flex-1 flex-col gap-4 overflow-y-auto p-5">
          <div className="flex gap-3 rounded-lg border bg-secondary p-3">
            {isMint ? (
              <ShieldCheckIcon className="mt-0.5 size-4 shrink-0 text-instrument-nominal" />
            ) : (
              <LockIcon className="mt-0.5 size-4 shrink-0 text-instrument-nominal" />
            )}
            <div className="text-[0.78rem] leading-relaxed text-muted-foreground">
              The new value is sealed in the org secret store and used from the next session on.
            </div>
          </div>

          {isMint ? (
            (mintKind?.fields ?? []).map((f) => (
              <label key={f.name} className="flex flex-col gap-1.5">
                <span className="flex items-baseline gap-2">
                  <Text variant="label">{f.label}</Text>
                  {f.fieldKind === MintFieldKind.SEALED_SECRET && (
                    <span className="text-[0.68rem] text-muted-foreground">
                      •••• set · leave blank to keep
                    </span>
                  )}
                </span>
                {f.fieldKind === MintFieldKind.SEALED_SECRET ? (
                  <SecretField
                    value={values[f.name] ?? ""}
                    onChange={(v) => setValues((s) => ({ ...s, [f.name]: v }))}
                  />
                ) : (
                  <input
                    className="h-9 rounded-md border bg-transparent px-3 font-mono text-sm"
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
                  <span className="text-[0.68rem] text-muted-foreground">
                    •••• set · leave blank to keep
                  </span>
                </span>
                <SecretField
                  value={injectSecrets[inj.secretRef] ?? ""}
                  onChange={(v) => setInjectSecrets((s) => ({ ...s, [inj.secretRef]: v }))}
                  placeholder="••••••"
                />
              </label>
            ))
          )}

          {view.usedBy > 0 && (
            <div className="flex gap-2 rounded-md border border-instrument-caution/45 bg-instrument-caution/10 p-3">
              <InfoIcon className="mt-0.5 size-3.5 shrink-0 text-instrument-caution" />
              <span className="text-[0.74rem] leading-relaxed text-muted-foreground">
                {view.usedBy} profile{view.usedBy === 1 ? "" : "s"} use this integration. Running
                sessions keep their current credential; new sessions pick up the replacement.
              </span>
            </div>
          )}
          <div className="flex items-center gap-3">
            <Button variant="outline" onClick={runTest} disabled={!canSave || test.isPending}>
              {test.isPending ? (
                <>
                  <Loader2Icon className="size-4 animate-spin" />
                  Testing…
                </>
              ) : (
                <>
                  <ZapIcon className="size-4" />
                  Test new credential
                </>
              )}
            </Button>
            {testState === "ok" && (
              <span className="inline-flex items-center gap-1.5 text-[0.78rem] text-instrument-nominal">
                <CircleCheckIcon className="size-3.5" />
                <span className="font-mono text-muted-foreground">{testMessage}</span>
              </span>
            )}
            {testState === "fail" && (
              <span className="text-[0.78rem] text-destructive">{testMessage}</span>
            )}
          </div>

          {error && <p className="text-sm text-destructive">{error}</p>}
        </div>

        <footer className="flex items-center justify-end gap-2 border-t p-5">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={save} disabled={!canSave || pending}>
            <CheckIcon className="size-3.5" />
            {pending ? "Replacing…" : "Replace credential"}
          </Button>
        </footer>
      </SheetContent>
    </Sheet>
  );
}
