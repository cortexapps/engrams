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

// ADR 0058: a connector may inject several headers, each backed by its own org
// secret (most APIs need one; some, e.g. Datadog, need DD-API-KEY + DD-APPLICATION-KEY).
let nextCredId = 1;
interface CredRow {
  id: number;
  header: string;
  template: string;
  secretRef: string;
  credVal: string;
}
const newCred = (): CredRow => ({
  id: nextCredId++,
  header: "Authorization",
  template: "Bearer {}",
  secretRef: "",
  credVal: "",
});
/** A safe default org-secret name from a header (`DD-API-KEY` → `dd-api-key`). */
const sanitizeRef = (s: string) =>
  s
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");

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
  const [creds, setCreds] = useState<CredRow[]>([newCred()]);
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
  const valid = Boolean(
    name.trim() && hosts.trim() && ops.some((o) => o.grant.trim()) && creds.some((c) => c.header.trim()),
  );
  const pending = upsert.isPending || putSecret.isPending || uploadLogo.isPending;
  const updateCred = (id: number, patch: Partial<CredRow>) =>
    setCreds((r) => r.map((c) => (c.id === id ? { ...c, ...patch } : c)));

  const save = async () => {
    setError(null);
    // Each header gets its own org secret. A blank name defaults to
    // `${provider}-token` for a single header, else `${provider}-<header>`.
    const single = creds.filter((c) => c.header.trim()).length === 1;
    const resolved = creds
      .filter((c) => c.header.trim())
      .map((c) => ({
        ...c,
        ref: c.secretRef.trim() || (single ? `${provider}-token` : `${provider}-${sanitizeRef(c.header)}`),
      }));
    const connector = {
      provider,
      protocol: "http",
      display: { name: name.trim(), category, icon: { mono: monogram, color } },
      credential: {
        source: "inject",
        injects: resolved.map((c) => ({ header: c.header.trim(), secretRef: c.ref, template: c.template })),
      },
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
      for (const c of resolved) {
        if (c.credVal.trim()) await putSecret.mutateAsync({ name: c.ref, value: c.credVal });
      }
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

          {/* credential headers — one or more, each backed by its own org secret */}
          <div className="flex flex-col gap-2">
            <Text variant="label">Credential header(s)</Text>
            <p className="text-[0.74rem] leading-relaxed text-muted-foreground">
              Each header is injected at the egress proxy from its own org secret. Most APIs need
              one; some (e.g. Datadog) need several.
            </p>
            {creds.map((c) => (
              <div
                key={c.id}
                className="relative grid grid-cols-2 gap-3 rounded-md border bg-card/40 p-3"
              >
                <label className="flex flex-col gap-1.5">
                  <Text variant="label">Header</Text>
                  <Input
                    className="font-mono"
                    value={c.header}
                    onChange={(e) => updateCred(c.id, { header: e.target.value })}
                  />
                </label>
                <label className="flex flex-col gap-1.5">
                  <Text variant="label">Template</Text>
                  <Input
                    className="font-mono"
                    value={c.template}
                    onChange={(e) => updateCred(c.id, { template: e.target.value })}
                  />
                </label>
                <label className="flex flex-col gap-1.5">
                  <Text variant="label">Org-secret name</Text>
                  <Input
                    className="font-mono"
                    placeholder={`${provider}-token`}
                    spellCheck={false}
                    value={c.secretRef}
                    onChange={(e) => updateCred(c.id, { secretRef: e.target.value })}
                  />
                </label>
                <label className="flex flex-col gap-1.5">
                  <Text variant="label">Credential value</Text>
                  <SecretField
                    value={c.credVal}
                    onChange={(v) => updateCred(c.id, { credVal: v })}
                    placeholder="•••••"
                  />
                </label>
                {creds.length > 1 && (
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    aria-label="remove header"
                    className="absolute right-1 top-1 size-7 p-0"
                    onClick={() => setCreds((r) => r.filter((x) => x.id !== c.id))}
                  >
                    <XIcon className="size-3.5" />
                  </Button>
                )}
              </div>
            ))}
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="self-start"
              onClick={() => setCreds((r) => [...r, newCred()])}
            >
              <PlusIcon className="size-3.5" />
              Header
            </Button>
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
