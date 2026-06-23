import { Plus, TriangleAlert, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FieldDescription } from "@/components/ui/field";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

// ADR 0057: a profile secret row. `ref` names an org secret (the value-store
// key — the value itself lives in the org secret store, never here); `envVar`
// is the name the value is exposed as in the guest; `mode` is broker
// (placeholder + proxy substitution on `allowHosts`) or literal (raw env value).
export type SecretRow = {
  id: string;
  ref: string;
  envVar: string;
  mode: "broker" | "literal";
  allowHostsText: string; // comma/space/newline separated (broker mode)
};

let nextSecretRowId = 0;
const newSecretRowId = () => `secret-${nextSecretRowId++}`;

export function newSecretRow(): SecretRow {
  return { id: newSecretRowId(), ref: "", envVar: "", mode: "broker", allowHostsText: "" };
}

export interface ProfileSecretValue {
  ref: string;
  envVar: string;
  mode: "broker" | "literal";
  allowHosts: string[];
  allowHostPatterns: string[];
}

/** Split a comma/space/newline-separated host list. */
export function splitHosts(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/** SecretRow[] → the wire ProfileSecret[] (drops rows without a ref). */
export function secretRowsToWire(rows: SecretRow[]): ProfileSecretValue[] {
  return rows
    .filter((r) => r.ref.trim())
    .map((r) => ({
      ref: r.ref.trim(),
      envVar: r.envVar.trim(),
      mode: r.mode,
      allowHosts: r.mode === "broker" ? splitHosts(r.allowHostsText) : [],
      allowHostPatterns: [],
    }));
}

/** Wire ProfileSecret[] → editor rows (hydrate on edit). */
export function wireToSecretRows(
  secrets: ReadonlyArray<{
    ref: string;
    envVar: string;
    mode: string;
    allowHosts: string[];
  }>,
): SecretRow[] {
  return secrets.map((s) => ({
    id: newSecretRowId(),
    ref: s.ref,
    envVar: s.envVar,
    mode: s.mode === "literal" ? "literal" : "broker",
    allowHostsText: (s.allowHosts ?? []).join(", "),
  }));
}

const ENV_NAME_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

export function ProfileSecretsEditor({
  rows,
  onChange,
  secretNames,
}: {
  rows: SecretRow[];
  onChange: (rows: SecretRow[]) => void;
  /** Existing org-secret names for the ref typeahead (datalist). */
  secretNames: string[];
}) {
  const set = (id: string, patch: Partial<SecretRow>) =>
    onChange(rows.map((r) => (r.id === id ? { ...r, ...patch } : r)));

  // Surface duplicate env vars (the orchestrator rejects them on save).
  const counts = new Map<string, number>();
  for (const r of rows) {
    const e = r.envVar.trim();
    if (e) counts.set(e, (counts.get(e) ?? 0) + 1);
  }
  const dupes = [...counts.entries()].filter(([, n]) => n > 1).map(([k]) => k);
  const badEnv = rows.some((r) => r.envVar.trim() && !ENV_NAME_RE.test(r.envVar.trim()));

  return (
    <div className="flex flex-col gap-2">
      <datalist id="org-secret-names">
        {secretNames.map((n) => (
          <option key={n} value={n} />
        ))}
      </datalist>
      {rows.map((r) => (
        <div
          key={r.id}
          className="flex flex-col gap-2 rounded-md border p-2"
          data-testid="secret-row"
        >
          <div className="flex items-center gap-2">
            <Input
              className="font-mono"
              list="org-secret-names"
              placeholder="org-secret name (ref)"
              value={r.ref}
              spellCheck={false}
              autoCapitalize="off"
              aria-label="Org secret ref"
              onChange={(e) => set(r.id, { ref: e.target.value })}
            />
            <Input
              className="font-mono"
              placeholder="ENV_VAR"
              value={r.envVar}
              spellCheck={false}
              autoCapitalize="off"
              aria-label="Env var name"
              onChange={(e) => set(r.id, { envVar: e.target.value })}
            />
            <Select
              value={r.mode}
              onValueChange={(v) => set(r.id, { mode: v as "broker" | "literal" })}
            >
              <SelectTrigger className="w-32" aria-label="Injection mode">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="broker">broker</SelectItem>
                <SelectItem value="literal">literal</SelectItem>
              </SelectContent>
            </Select>
            <Button
              type="button"
              variant="ghost"
              size="icon"
              aria-label="Remove"
              onClick={() => onChange(rows.filter((x) => x.id !== r.id))}
            >
              <X className="size-4" />
            </Button>
          </div>
          {r.mode === "broker" && (
            <Input
              className="font-mono"
              placeholder="substitution hosts (e.g. api.datadoghq.com, db.internal)"
              value={r.allowHostsText}
              spellCheck={false}
              aria-label="Substitution hosts"
              onChange={(e) => set(r.id, { allowHostsText: e.target.value })}
            />
          )}
        </div>
      ))}
      <Button
        type="button"
        variant="outline"
        size="sm"
        className="self-start"
        onClick={() => onChange([...rows, newSecretRow()])}
      >
        <Plus className="size-4" /> Add secret
      </Button>

      {(dupes.length > 0 || badEnv) && (
        <p className="flex items-center gap-1.5 text-xs text-foreground">
          <TriangleAlert className="size-3.5 shrink-0 text-instrument-caution" />
          {dupes.length > 0
            ? `Duplicate env var${dupes.length === 1 ? "" : "s"} (${dupes.join(", ")}).`
            : "An env var name is invalid (use [A-Za-z_][A-Za-z0-9_]*)."}
        </p>
      )}

      <FieldDescription>
        Each secret resolves its value from the org secret store by <code>ref</code> at session
        create. <strong>broker</strong> injects a placeholder the egress proxy substitutes only on
        the listed hosts (the guest never holds the value); <strong>literal</strong> puts the raw
        value in the env. Enter org secrets in Settings → Integrations.
      </FieldDescription>
    </div>
  );
}
