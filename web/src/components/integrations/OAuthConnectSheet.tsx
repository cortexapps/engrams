/**
 * OAuthConnectSheet — the "Add to {provider}" flow for connectors with an OAuth
 * facet (e.g. Slack). Unlike mint/inject, the credential isn't hand-entered: the
 * admin registers their own OAuth app (BYO), pastes its client id + secret (sealed
 * as org secrets), and is redirected to the provider's consent screen. The access
 * token is obtained + stored server-side on the callback.
 */

import { useState } from "react";
import { ArrowUpRightIcon, InfoIcon, LockIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Sheet, SheetContent } from "@/components/ui/sheet";
import { Text } from "@/components/ui/text";
import { usePutOrgSecret } from "@/hooks/useOrgSecrets";
import type { ParsedOauth } from "@/lib/connectorModel";
import { ProviderTile } from "./ProviderTile";
import { SecretField } from "./SecretField";
import type { ConnectorView } from "./useConnectorViews";

export function OAuthConnectSheet({
  view,
  oauth,
  onClose,
}: {
  view: ConnectorView;
  oauth: ParsedOauth;
  onClose: () => void;
}) {
  const putSecret = usePutOrgSecret();
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const redirectUri = `${window.location.origin}/api/v1/integrations/${view.provider}/oauth/callback`;
  const filled = clientId.trim().length > 0 && clientSecret.trim().length > 0;

  const connect = async () => {
    setError(null);
    setBusy(true);
    try {
      await putSecret.mutateAsync({ name: oauth.clientIdRef, value: clientId.trim() });
      await putSecret.mutateAsync({ name: oauth.clientSecretRef, value: clientSecret.trim() });
      // Leave the SPA for the provider's consent screen; the callback redirects back.
      window.location.href = `/api/v1/integrations/${encodeURIComponent(view.provider)}/oauth/authorize`;
    } catch (e) {
      setBusy(false);
      setError(e instanceof Error ? e.message : String(e));
    }
  };

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

        <div className="flex flex-1 flex-col gap-4 overflow-y-auto p-5">
          <div className="flex gap-3 rounded-lg border bg-secondary p-3">
            <LockIcon className="mt-0.5 size-4 shrink-0 text-instrument-nominal" />
            <div>
              <div className="text-sm font-semibold">Bring your own {view.name} app</div>
              <div className="mt-0.5 text-[0.78rem] leading-relaxed text-muted-foreground">
                Create an app in {view.name}, add this redirect URL, then paste its client
                credentials below. You'll approve access on {view.name}; the workspace token is
                obtained and sealed server-side — it never reaches the browser.
              </div>
            </div>
          </div>

          <label className="flex flex-col gap-1.5">
            <Text variant="label">Redirect URL (add this to your {view.name} app)</Text>
            <code className="select-all rounded-md border bg-card px-3 py-2 font-mono text-[0.72rem] break-all">
              {redirectUri}
            </code>
          </label>

          <label className="flex flex-col gap-1.5">
            <span className="flex items-baseline gap-2">
              <Text variant="label">Client ID</Text>
              <code className="font-mono text-[0.62rem] text-muted-foreground">
                {oauth.clientIdRef}
              </code>
            </span>
            <Input
              className="font-mono"
              autoComplete="off"
              spellCheck={false}
              value={clientId}
              onChange={(e) => setClientId(e.target.value)}
            />
          </label>

          <label className="flex flex-col gap-1.5">
            <span className="flex items-baseline gap-2">
              <Text variant="label">Client secret</Text>
              <code className="font-mono text-[0.62rem] text-muted-foreground">
                {oauth.clientSecretRef}
              </code>
            </span>
            <SecretField value={clientSecret} onChange={setClientSecret} placeholder="••••••" />
          </label>

          <p className="flex gap-2 text-[0.74rem] text-muted-foreground">
            <InfoIcon className="mt-0.5 size-3.5 shrink-0" />
            The client id + secret are sealed in the org secret store; the obtained access token is
            stored as the connector's injected credential.
          </p>

          {error && <p className="text-sm text-destructive">{error}</p>}
        </div>

        <footer className="flex items-center justify-between gap-2 border-t p-5">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={connect} disabled={!filled || busy}>
            {busy ? "Redirecting…" : `Add ${view.name}`}
            <ArrowUpRightIcon className="size-3.5" />
          </Button>
        </footer>
      </SheetContent>
    </Sheet>
  );
}
