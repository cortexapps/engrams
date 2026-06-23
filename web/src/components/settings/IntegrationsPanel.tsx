import { useState } from "react";
import {
  useConnectors,
  useDeleteConnector,
  useMintKinds,
  useUpsertConnector,
} from "../../hooks/useIntegrations";
import { usePutOrgSecret } from "../../hooks/useOrgSecrets";
import { MintFieldKind, type MintKind } from "../../gen/engram/app/v1/mint_pb";
import type { Connector } from "../../gen/engram/app/v1/integration_pb";
import { PageHeading } from "../page-heading";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";

// Integrations (ADR 0057 C4) — the admin catalog of providers a profile's
// capabilities can unlock. Two planes:
//   - Plane A (mint): the platform issues short-lived capability-scoped creds
//     (e.g. a GitHub App). The form is data-driven from the coordinator's
//     mint-kind registry; each field is stored as an org secret (`<kind>.<field>`),
//     which is exactly where the coordinator resolves it at mint time.
//   - Plane B (inject): a declarative connector (host + header + operations) plus
//     a write-only credential stored as an org secret. Built-ins are read-only.
export function IntegrationsPanel() {
  const { data, isLoading, error } = useConnectors();
  const rows = data?.connectors ?? [];

  return (
    <div className="space-y-6">
      <PageHeading
        title="Integrations"
        description="Connectors define what a capability unlocks. Mint providers (Plane A) issue scoped credentials; inject connectors (Plane B) attach a stored credential to specific API calls. Built-ins are read-only."
        actions={
          <div className="flex gap-2">
            <AddMintProviderDialog />
            <AddConnectorDialog />
          </div>
        }
      />

      {error && (
        <p className="text-sm text-destructive">could not load integrations — {String(error)}</p>
      )}

      {isLoading ? (
        <p className="py-6 text-sm text-muted-foreground">Loading…</p>
      ) : rows.length === 0 ? (
        <Card>
          <CardContent className="py-10 text-center text-sm text-muted-foreground">
            No connectors yet. Add a mint provider (e.g. GitHub) or an inject connector.
          </CardContent>
        </Card>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Provider</TableHead>
              <TableHead>Credential</TableHead>
              <TableHead>Source</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((row) => (
              <ConnectorRow key={row.provider} row={row} />
            ))}
          </TableBody>
        </Table>
      )}
    </div>
  );
}

/** Read the credential source ("mint" | "inject") out of a connector's JSON. */
function credentialSource(configJson: string): string {
  try {
    const c = JSON.parse(configJson) as { credential?: { source?: string } };
    return c.credential?.source ?? "—";
  } catch {
    return "—";
  }
}

function ConnectorRow({ row }: { row: Connector }) {
  const del = useDeleteConnector();
  return (
    <TableRow>
      <TableCell className="font-mono text-sm">{row.provider}</TableCell>
      <TableCell>
        <Badge variant="outline">{credentialSource(row.configJson)}</Badge>
      </TableCell>
      <TableCell>
        <Badge variant={row.builtin ? "secondary" : "outline"}>
          {row.builtin ? "built-in" : "custom"}
        </Badge>
      </TableCell>
      <TableCell>
        <div className="flex items-center justify-end">
          {row.builtin ? (
            <span className="text-xs text-muted-foreground">read-only</span>
          ) : (
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button variant="ghost" size="sm" disabled={del.isPending}>
                  Remove
                </Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Remove {row.provider}?</AlertDialogTitle>
                  <AlertDialogDescription>
                    Profiles granting <code>{row.provider}:*</code> capabilities will stop unlocking
                    them on the next session create. The stored credential is not deleted.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction onClick={() => del.mutate({ provider: row.provider })}>
                    Remove connector
                  </AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          )}
        </div>
        {del.error && (
          <p className="mt-1 text-right text-xs text-destructive">
            could not remove — {String(del.error)}
          </p>
        )}
      </TableCell>
    </TableRow>
  );
}

// --- Plane A: data-driven mint-provider form -------------------------------

function AddMintProviderDialog() {
  const [open, setOpen] = useState(false);
  const { data } = useMintKinds();
  const kinds = data?.mintKinds ?? [];
  const put = usePutOrgSecret();
  const [kindKey, setKindKey] = useState("");
  const [values, setValues] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);

  const selected: MintKind | undefined = kinds.find((k) => k.kind === kindKey) ?? kinds[0];

  const reset = () => {
    setValues({});
    setError(null);
    setKindKey("");
  };

  const onSubmit = async () => {
    if (!selected) return;
    for (const f of selected.fields) {
      if (f.required && !(values[f.name] ?? "").trim()) {
        setError(`${f.label} is required`);
        return;
      }
    }
    try {
      // Each field is stored as an org secret named `<kind>.<field>` — exactly
      // where the coordinator resolves it when it builds the mint engine.
      for (const f of selected.fields) {
        const v = values[f.name] ?? "";
        if (!v.trim()) continue;
        await put.mutateAsync({ name: `${selected.kind}.${f.name}`, value: v });
      }
      reset();
      setOpen(false);
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (o) reset();
      }}
    >
      <DialogTrigger asChild>
        <Button variant="outline">Add mint provider</Button>
      </DialogTrigger>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Connect a mint provider</DialogTitle>
          <DialogDescription>
            Plane A: the platform issues short-lived, capability-scoped credentials (e.g. a GitHub
            App). Each value is sealed in the org secret store and never returned.
          </DialogDescription>
        </DialogHeader>

        {kinds.length === 0 ? (
          <p className="py-4 text-sm text-muted-foreground">No mint kinds are available.</p>
        ) : (
          <FieldGroup>
            {kinds.length > 1 && (
              <Field>
                <FieldLabel htmlFor="mint-kind">Provider</FieldLabel>
                <Select value={selected?.kind ?? ""} onValueChange={setKindKey}>
                  <SelectTrigger id="mint-kind" aria-label="Mint provider">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {kinds.map((k) => (
                      <SelectItem key={k.kind} value={k.kind}>
                        {k.displayName}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </Field>
            )}
            {selected?.fields.map((f) => (
              <Field key={f.name}>
                <FieldLabel htmlFor={`mint-${f.name}`}>{f.label}</FieldLabel>
                <Input
                  id={`mint-${f.name}`}
                  type={f.fieldKind === MintFieldKind.SEALED_SECRET ? "password" : "text"}
                  className="font-mono"
                  autoComplete="off"
                  spellCheck={false}
                  value={values[f.name] ?? ""}
                  onChange={(e) => setValues((v) => ({ ...v, [f.name]: e.target.value }))}
                />
                <FieldDescription>
                  Stored as org secret{" "}
                  <code className="font-mono">{`${selected.kind}.${f.name}`}</code>.
                </FieldDescription>
              </Field>
            ))}
            {error && <FieldError errors={[{ message: error }]} />}
          </FieldGroup>
        )}

        <DialogFooter className="mt-5">
          <Button
            type="button"
            variant="ghost"
            onClick={() => setOpen(false)}
            disabled={put.isPending}
          >
            Cancel
          </Button>
          <Button type="button" onClick={onSubmit} disabled={put.isPending || !selected}>
            {put.isPending ? "Sealing & saving…" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// --- Plane B: declarative connector form-builder ---------------------------

let nextOpId = 1;
interface OpRow {
  id: number;
  grants: string;
  method: string;
  path: string;
}
function newOpRow(): OpRow {
  return { id: nextOpId++, grants: "", method: "GET", path: "" };
}

function splitList(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

function AddConnectorDialog() {
  const [open, setOpen] = useState(false);
  const upsert = useUpsertConnector();
  const put = usePutOrgSecret();
  const [provider, setProvider] = useState("");
  const [hosts, setHosts] = useState("");
  const [header, setHeader] = useState("Authorization");
  const [secretRef, setSecretRef] = useState("");
  const [template, setTemplate] = useState("Bearer {}");
  const [credentialValue, setCredentialValue] = useState("");
  const [ops, setOps] = useState<OpRow[]>([newOpRow()]);
  const [error, setError] = useState<string | null>(null);

  const reset = () => {
    setProvider("");
    setHosts("");
    setHeader("Authorization");
    setSecretRef("");
    setTemplate("Bearer {}");
    setCredentialValue("");
    setOps([newOpRow()]);
    setError(null);
  };

  const onSubmit = async () => {
    const connector = {
      provider: provider.trim(),
      protocol: "http",
      credential: {
        source: "inject",
        inject: { header: header.trim(), secretRef: secretRef.trim(), template },
      },
      hosts: splitList(hosts),
      operations: ops.map((o) => {
        const match: Record<string, string> = {};
        if (o.method.trim()) match.method = o.method.trim();
        if (o.path.trim()) match.path = o.path.trim();
        return {
          grants: splitList(o.grants),
          ...(Object.keys(match).length > 0 ? { match } : {}),
        };
      }),
    };
    try {
      // The connector is validated server-side (parseConnector — the admin-trust
      // boundary); a bad shape surfaces as the error below.
      await upsert.mutateAsync({ configJson: JSON.stringify(connector) });
      // The write-only credential the connector's secretRef points at.
      if (credentialValue.trim()) {
        await put.mutateAsync({ name: secretRef.trim(), value: credentialValue });
      }
      reset();
      setOpen(false);
    } catch (e) {
      setError(String(e));
    }
  };

  const pending = upsert.isPending || put.isPending;

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (o) reset();
      }}
    >
      <DialogTrigger asChild>
        <Button>Add connector</Button>
      </DialogTrigger>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>New connector</DialogTitle>
          <DialogDescription>
            Plane B: attach a stored credential to specific API calls. The credential is sealed in
            the org secret store; the connector config is validated before it's saved.
          </DialogDescription>
        </DialogHeader>

        <FieldGroup>
          <Field>
            <FieldLabel htmlFor="conn-provider">Provider</FieldLabel>
            <Input
              id="conn-provider"
              className="font-mono"
              placeholder="sentry"
              spellCheck={false}
              autoCapitalize="off"
              value={provider}
              onChange={(e) => setProvider(e.target.value)}
            />
            <FieldDescription>
              The capability namespace (
              <code className="font-mono">{`${provider || "sentry"}:action`}</code>).
            </FieldDescription>
          </Field>

          <Field>
            <FieldLabel htmlFor="conn-hosts">Hosts</FieldLabel>
            <Input
              id="conn-hosts"
              className="font-mono"
              placeholder="sentry.io, *.sentry.io"
              spellCheck={false}
              value={hosts}
              onChange={(e) => setHosts(e.target.value)}
            />
            <FieldDescription>
              Comma/space separated. Egress opens to these for matched calls.
            </FieldDescription>
          </Field>

          <div className="grid grid-cols-2 gap-3">
            <Field>
              <FieldLabel htmlFor="conn-header">Header</FieldLabel>
              <Input
                id="conn-header"
                className="font-mono"
                value={header}
                onChange={(e) => setHeader(e.target.value)}
              />
            </Field>
            <Field>
              <FieldLabel htmlFor="conn-template">Template</FieldLabel>
              <Input
                id="conn-template"
                className="font-mono"
                value={template}
                onChange={(e) => setTemplate(e.target.value)}
              />
              <FieldDescription>
                <code className="font-mono">{"{}"}</code> is replaced by the credential.
              </FieldDescription>
            </Field>
          </div>

          <Field>
            <FieldLabel htmlFor="conn-ref">Org-secret name (ref)</FieldLabel>
            <Input
              id="conn-ref"
              className="font-mono"
              placeholder="sentry-token"
              spellCheck={false}
              value={secretRef}
              onChange={(e) => setSecretRef(e.target.value)}
            />
          </Field>

          <Field>
            <FieldLabel htmlFor="conn-cred">Credential value (write-only)</FieldLabel>
            <Input
              id="conn-cred"
              type="password"
              className="font-mono"
              autoComplete="new-password"
              placeholder="•••••"
              value={credentialValue}
              onChange={(e) => setCredentialValue(e.target.value)}
            />
            <FieldDescription>
              Sealed in the org secret store under the ref above. Never returned.
            </FieldDescription>
          </Field>

          <div className="space-y-2">
            <FieldLabel>Operations</FieldLabel>
            {ops.map((o) => (
              <div key={o.id} className="grid grid-cols-[1fr_5rem_1fr_2rem] items-center gap-2">
                <Input
                  className="font-mono"
                  placeholder="issues:read"
                  aria-label="grants"
                  value={o.grants}
                  onChange={(e) =>
                    setOps((rows) =>
                      rows.map((r) => (r.id === o.id ? { ...r, grants: e.target.value } : r)),
                    )
                  }
                />
                <Input
                  className="font-mono"
                  placeholder="GET"
                  aria-label="method"
                  value={o.method}
                  onChange={(e) =>
                    setOps((rows) =>
                      rows.map((r) => (r.id === o.id ? { ...r, method: e.target.value } : r)),
                    )
                  }
                />
                <Input
                  className="font-mono"
                  placeholder="/api/0/projects/*/issues/"
                  aria-label="path"
                  value={o.path}
                  onChange={(e) =>
                    setOps((rows) =>
                      rows.map((r) => (r.id === o.id ? { ...r, path: e.target.value } : r)),
                    )
                  }
                />
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  aria-label="remove operation"
                  onClick={() =>
                    setOps((rows) => (rows.length > 1 ? rows.filter((r) => r.id !== o.id) : rows))
                  }
                >
                  ✕
                </Button>
              </div>
            ))}
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => setOps((r) => [...r, newOpRow()])}
            >
              + Operation
            </Button>
            <FieldDescription>
              Each operation maps a capability action (its grants) to the API calls it unlocks.
            </FieldDescription>
          </div>

          {error && <FieldError errors={[{ message: error }]} />}
        </FieldGroup>

        <DialogFooter className="mt-5">
          <Button type="button" variant="ghost" onClick={() => setOpen(false)} disabled={pending}>
            Cancel
          </Button>
          <Button type="button" onClick={onSubmit} disabled={pending}>
            {pending ? "Saving…" : "Save connector"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
