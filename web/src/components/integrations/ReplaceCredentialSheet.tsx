/**
 * ReplaceCredentialSheet — rotate a connected provider's credential (right Sheet).
 * mint → re-seal the kind's fields (blank = keep); inject → overwrite the org
 * secret. The new value is used from the next session on; running sessions keep
 * their current credential. (A "Test new credential" affordance lands with the
 * coordinator TestConnector RPC.)
 */

import { useState } from "react";
import { toast } from "sonner";
import { CheckIcon, InfoIcon, LockIcon, ShieldCheckIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Sheet, SheetContent } from "@/components/ui/sheet";
import { Text } from "@/components/ui/text";
import { MintFieldKind } from "@/gen/engram/app/v1/mint_pb";
import { useConnectors, useMintKinds, useSetMintCredential } from "@/hooks/useIntegrations";
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

  const isMint = view.credentialSource === "mint";
  const row = conns.data?.connectors.find((c) => c.provider === view.provider);
  const cfg = row ? parseConnectorConfig(row.configJson, view.provider) : undefined;
  const mintKind = isMint
    ? mintKinds.data?.mintKinds.find(
        (k) => k.provider === view.provider || k.kind === cfg?.mintKind,
      )
    : undefined;

  const [values, setValues] = useState<Record<string, string>>({});
  const [injectSecret, setInjectSecret] = useState("");
  const [error, setError] = useState<string | null>(null);

  const canSave = isMint
    ? Object.values(values).some((v) => v.trim().length > 0)
    : injectSecret.trim().length > 0;
  const pending = setMint.isPending || putSecret.isPending;

  const save = async () => {
    setError(null);
    try {
      if (isMint) {
        if (!mintKind) throw new Error("no mint kind for this provider");
        // Blank fields are skipped server-side (leave existing unchanged).
        await setMint.mutateAsync({ provider: view.provider, kind: mintKind.kind, values });
      } else {
        if (!cfg?.secretRef) throw new Error("connector has no secret ref");
        await putSecret.mutateAsync({ name: cfg.secretRef, value: injectSecret });
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
                : `Rotate the org secret · ${cfg?.secretRef ?? ""}`}
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
            <label className="flex flex-col gap-1.5">
              <span className="flex items-baseline gap-2">
                <Text variant="label">{view.name} credential</Text>
                <span className="text-[0.68rem] text-muted-foreground">
                  •••• set · leave blank to keep
                </span>
              </span>
              <SecretField value={injectSecret} onChange={setInjectSecret} placeholder="••••••" />
            </label>
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
