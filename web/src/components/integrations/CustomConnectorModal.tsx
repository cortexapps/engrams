/**
 * CustomConnectorModal — author a brokered (inject-only) connector (spec §E).
 *
 * Mint providers are built-in only, so `credential.source` is forced to `inject`.
 * Collects the provider identity (monogram + color + optional logo), the egress
 * hosts, the header/template/org-secret + write-only credential, and the
 * operations grid. The connector JSON is validated server-side by `parseConnector`
 * (the admin-trust boundary); the logo bytes upload after the connector exists.
 */

import { useMemo, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  CornerDownRightIcon,
  ExternalLinkIcon,
  LockIcon,
  PlusIcon,
  UploadIcon,
  XIcon,
} from "lucide-react";

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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Text } from "@/components/ui/text";
import { useUpsertConnector, useUploadConnectorLogo } from "@/hooks/useIntegrations";
import { usePutOrgSecret } from "@/hooks/useOrgSecrets";
import { accessOf, defaultIconMono, humanizeAction } from "@/lib/connectorModel";
import { ProviderTile } from "./ProviderTile";
import { AccessTag } from "./chips";
import { SecretField } from "./SecretField";

const CATEGORIES = [
  "Source control",
  "Observability",
  "Incident",
  "Communication",
  "Project tracking",
  "Security",
];
const PALETTE = [
  "#4c4a73",
  "#1f2328",
  "#632ca6",
  "#0052cc",
  "#06ac38",
  "#b8324f",
  "#c4622d",
  "#2a6f6f",
];

let nextOpId = 1;
interface OpRow {
  id: number;
  grant: string;
  method: string;
  path: string;
}
const newOp = (): OpRow => ({ id: nextOpId++, grant: "", method: "GET", path: "" });
const splitList = (t: string) =>
  t
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
export function CustomConnectorModal({ onClose }: { onClose: () => void }) {
  const navigate = useNavigate();
  const upsert = useUpsertConnector();
  const putSecret = usePutOrgSecret();
  const uploadLogo = useUploadConnectorLogo();

  const [name, setName] = useState("");
  const [category, setCategory] = useState(CATEGORIES[1]!);
  const [hosts, setHosts] = useState("");
  const [header, setHeader] = useState("Authorization");
  const [template, setTemplate] = useState("Bearer {}");
  const [secretRef, setSecretRef] = useState("");
  const [credVal, setCredVal] = useState("");
  const [ops, setOps] = useState<OpRow[]>([newOp()]);
  const [mono, setMono] = useState("");
  const [color, setColor] = useState(PALETTE[0]!);
  const [logo, setLogo] = useState<File | null>(null);
  const [error, setError] = useState<string | null>(null);

  const provider =
    name
      .trim()
      .toLowerCase()
      .replace(/[^a-z0-9_-]/g, "") || "provider";
  const autoMono = defaultIconMono(name.trim() || "?");
  const monogram = (mono.trim() || autoMono).slice(0, 2).toUpperCase();
  const logoUrl = useMemo(() => (logo ? URL.createObjectURL(logo) : undefined), [logo]);
  const valid = Boolean(name.trim() && hosts.trim() && ops.some((o) => o.grant.trim()));
  const pending = upsert.isPending || putSecret.isPending || uploadLogo.isPending;

  const save = async () => {
    setError(null);
    const ref = secretRef.trim() || `${provider}-token`;
    const connector = {
      provider,
      protocol: "http",
      display: { name: name.trim(), category, icon: { mono: monogram, color } },
      credential: { source: "inject", inject: { header: header.trim(), secretRef: ref, template } },
      hosts: splitList(hosts),
      operations: ops
        .filter((o) => o.grant.trim())
        .map((o) => {
          const match: Record<string, string> = {};
          if (o.method.trim()) match.method = o.method.trim();
          if (o.path.trim()) match.path = o.path.trim();
          return { grants: [o.grant.trim()], ...(Object.keys(match).length ? { match } : {}) };
        }),
    };
    try {
      await upsert.mutateAsync({ configJson: JSON.stringify(connector) });
      if (credVal.trim()) await putSecret.mutateAsync({ name: ref, value: credVal });
      if (logo) {
        const data = new Uint8Array(await logo.arrayBuffer());
        await uploadLogo.mutateAsync({ provider, data, mediaType: logo.type });
      }
      toast.success(`${name.trim()} added`);
      onClose();
      void navigate({ to: "/settings/integrations/$provider", params: { provider } });
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Custom connector</DialogTitle>
          <DialogDescription>
            For any token-authenticated HTTP API. Map each power to the calls it unlocks.
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-col gap-4">
          <div className="grid grid-cols-2 gap-3">
            <label className="flex flex-col gap-1.5">
              <Text variant="label">Provider</Text>
              <Input
                className="font-mono"
                placeholder="sentry"
                spellCheck={false}
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <Text variant="label">Category</Text>
              <Select value={category} onValueChange={setCategory}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {CATEGORIES.map((c) => (
                    <SelectItem key={c} value={c}>
                      {c}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </label>
          </div>

          {/* identity */}
          <div className="flex flex-col gap-2">
            <Text variant="label">Icon</Text>
            <div className="flex items-center gap-4 rounded-md border bg-card p-3">
              <ProviderTile
                mono={monogram}
                color={color}
                {...(logoUrl ? { logo: logoUrl } : {})}
                size={46}
              />
              <div className="flex min-w-0 flex-1 flex-col gap-2">
                <div className="flex items-center gap-2">
                  <Input
                    className="w-16 text-center font-mono uppercase"
                    placeholder={autoMono}
                    maxLength={2}
                    value={mono}
                    onChange={(e) => setMono(e.target.value.slice(0, 2))}
                  />
                  <div className="flex flex-wrap gap-1.5">
                    {PALETTE.map((sw) => (
                      <button
                        key={sw}
                        type="button"
                        aria-label={`color ${sw}`}
                        onClick={() => setColor(sw)}
                        className="size-5 rounded-sm ring-offset-1"
                        style={{
                          background: sw,
                          outline: color === sw ? "2px solid var(--ring)" : "none",
                        }}
                      />
                    ))}
                  </div>
                </div>
                <div className="flex items-center gap-3">
                  <label className="inline-flex cursor-pointer items-center gap-1.5 text-[0.76rem] text-primary">
                    <UploadIcon className="size-3.5" />
                    {logo ? "Replace logo" : "Upload logo"}
                    <input
                      type="file"
                      accept="image/svg+xml,image/png"
                      className="hidden"
                      onChange={(e) => setLogo(e.target.files?.[0] ?? null)}
                    />
                  </label>
                  {logo && (
                    <button
                      type="button"
                      onClick={() => setLogo(null)}
                      className="inline-flex items-center gap-1 text-[0.76rem] text-muted-foreground"
                    >
                      <XIcon className="size-3" />
                      Remove
                    </button>
                  )}
                  <span className="text-[0.7rem] text-muted-foreground">
                    SVG or square PNG · falls back to the monogram
                  </span>
                </div>
              </div>
            </div>
          </div>

          <div className="flex items-center gap-2.5 rounded-md border bg-secondary p-3">
            <LockIcon className="size-4 shrink-0 text-instrument-nominal" />
            <span className="text-[0.78rem] leading-relaxed text-muted-foreground">
              <strong className="text-foreground">Brokered credential.</strong> The token is sealed
              as an org secret and substituted into the request at the egress proxy — it never
              enters the sandbox. Platform-minted providers (like GitHub) are built-in and can't be
              added here.
            </span>
          </div>

          <label className="flex flex-col gap-1.5">
            <Text variant="label">Hosts (egress)</Text>
            <Input
              className="font-mono"
              placeholder="sentry.io, *.sentry.io"
              spellCheck={false}
              value={hosts}
              onChange={(e) => setHosts(e.target.value)}
            />
          </label>

          <div className="grid grid-cols-2 gap-3">
            <label className="flex flex-col gap-1.5">
              <Text variant="label">Header</Text>
              <Input
                className="font-mono"
                value={header}
                onChange={(e) => setHeader(e.target.value)}
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <Text variant="label">Template</Text>
              <Input
                className="font-mono"
                value={template}
                onChange={(e) => setTemplate(e.target.value)}
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <Text variant="label">Org-secret name</Text>
              <Input
                className="font-mono"
                placeholder={`${provider}-token`}
                spellCheck={false}
                value={secretRef}
                onChange={(e) => setSecretRef(e.target.value)}
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <Text variant="label">Credential value</Text>
              <SecretField value={credVal} onChange={setCredVal} placeholder="•••••" />
            </label>
          </div>

          {/* operations */}
          <div className="flex flex-col gap-2">
            <Text variant="label">Operations</Text>
            <p className="text-[0.74rem] leading-relaxed text-muted-foreground">
              The <strong>action slug</strong> is the capability a profile grants; the{" "}
              <strong>method + path</strong> become the proxy's gate (the path is matched as a glob
              — <code className="font-mono">*</code> matches any characters). Read/write and the
              label are derived for display — not stored.
            </p>
            {ops.map((o) => {
              const g = o.grant.trim();
              return (
                <div key={o.id} className="flex flex-col gap-1.5">
                  <div className="grid grid-cols-[1.4fr_4.5rem_1.8fr_2rem] items-center gap-2">
                    <Input
                      className="font-mono text-xs"
                      placeholder="issues:read"
                      aria-label="action slug"
                      spellCheck={false}
                      value={o.grant}
                      onChange={(e) =>
                        setOps((r) =>
                          r.map((x) =>
                            x.id === o.id ? { ...x, grant: e.target.value.replace(/\s/g, "") } : x,
                          ),
                        )
                      }
                    />
                    <Input
                      className="font-mono text-xs"
                      placeholder="GET"
                      aria-label="method"
                      value={o.method}
                      onChange={(e) =>
                        setOps((r) =>
                          r.map((x) =>
                            x.id === o.id ? { ...x, method: e.target.value.toUpperCase() } : x,
                          ),
                        )
                      }
                    />
                    <Input
                      className="font-mono text-xs"
                      placeholder="/api/0/issues/*"
                      aria-label="path"
                      spellCheck={false}
                      value={o.path}
                      onChange={(e) =>
                        setOps((r) =>
                          r.map((x) => (x.id === o.id ? { ...x, path: e.target.value } : x)),
                        )
                      }
                    />
                    <Button
                      type="button"
                      variant="ghost"
                      size="sm"
                      aria-label="remove operation"
                      onClick={() =>
                        setOps((r) => (r.length > 1 ? r.filter((x) => x.id !== o.id) : r))
                      }
                    >
                      <XIcon className="size-3.5" />
                    </Button>
                  </div>
                  {g && (
                    <div className="flex flex-wrap items-center gap-2 pl-1 text-muted-foreground">
                      <CornerDownRightIcon className="size-3 opacity-60" />
                      <code className="rounded-full border bg-secondary px-2 py-px font-mono text-[0.72rem]">
                        {provider}:{g}
                      </code>
                      <span className="text-[0.74rem]">"{humanizeAction(g)}"</span>
                      <AccessTag access={accessOf(o.method)} />
                      {o.path && (
                        <span className="text-[0.72rem]">
                          · gate{" "}
                          <code className="font-mono">
                            {(o.method || "GET").toUpperCase()} {o.path}
                          </code>
                        </span>
                      )}
                    </div>
                  )}
                </div>
              );
            })}
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="self-start"
              onClick={() => setOps((r) => [...r, newOp()])}
            >
              <PlusIcon className="size-3.5" />
              Operation
            </Button>
          </div>

          {/* appears-as */}
          <div className="flex flex-col gap-2">
            <Text variant="label">Appears as</Text>
            <div className="flex flex-wrap items-center gap-4 rounded-md border border-dashed p-3">
              <span className="inline-flex items-center gap-2">
                <ProviderTile
                  mono={monogram}
                  color={color}
                  {...(logoUrl ? { logo: logoUrl } : {})}
                  size={28}
                />
                <span className="font-display text-sm font-semibold">
                  {name.trim() || "Provider"}
                </span>
              </span>
              <span className="h-5 w-px bg-border" />
              <span className="inline-flex items-center gap-2 text-sm">
                <ProviderTile
                  mono={monogram}
                  color={color}
                  {...(logoUrl ? { logo: logoUrl } : {})}
                  size={18}
                />
                <span>Created issue</span>
                <code className="font-mono text-xs text-muted-foreground">#1481</code>
                <ExternalLinkIcon className="size-3 text-muted-foreground" />
              </span>
            </div>
            <span className="text-[0.72rem] leading-relaxed text-muted-foreground">
              This mark follows the integration everywhere — the catalog, profile powers, the
              session policy, and in-session events.
            </span>
          </div>
          {error && <p className="text-sm text-destructive">{error}</p>}
        </div>

        <DialogFooter className="mt-2">
          <Button type="button" variant="ghost" onClick={onClose} disabled={pending}>
            Cancel
          </Button>
          <Button type="button" onClick={save} disabled={!valid || pending}>
            {pending ? "Adding…" : "Add connector"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
