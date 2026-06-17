import { useState } from "react";
import { Eye, EyeOff, Plus, TriangleAlert, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FieldDescription } from "@/components/ui/field";

export type EnvRow = { id: string; key: string; value: string };

// Stable per-row ids so React keys survive insert/delete. Keying by array
// index mis-associates inputs (and steals focus) when a middle row is removed.
let nextEnvRowId = 0;
const newEnvRowId = () => `env-${nextEnvRowId++}`;

export function envRowsToMap(rows: EnvRow[]): Record<string, string> {
  const out: Record<string, string> = {};
  // Last write wins on a duplicate key — the editor warns before this point.
  for (const r of rows) if (r.key.trim()) out[r.key.trim()] = r.value;
  return out;
}
export function mapToEnvRows(map: Record<string, string>): EnvRow[] {
  return Object.entries(map).map(([key, value]) => ({ id: newEnvRowId(), key, value }));
}

export function EnvVarsEditor({
  rows,
  onChange,
}: {
  rows: EnvRow[];
  onChange: (rows: EnvRow[]) => void;
}) {
  // Values default to visible (most are config like ANTHROPIC_MODEL); the eye
  // toggle hides a sensitive one while editing, so a secret isn't left in the
  // clear on screen.
  const [revealed, setRevealed] = useState<Record<string, boolean>>({});
  const set = (id: string, patch: Partial<EnvRow>) =>
    onChange(rows.map((r) => (r.id === id ? { ...r, ...patch } : r)));

  // Duplicate trimmed keys silently collapse (last wins) — surface them so the
  // admin isn't surprised which value survives.
  const counts = new Map<string, number>();
  for (const r of rows) {
    const k = r.key.trim();
    if (k) counts.set(k, (counts.get(k) ?? 0) + 1);
  }
  const dupes = [...counts.entries()].filter(([, n]) => n > 1).map(([k]) => k);

  return (
    <div className="flex flex-col gap-2">
      {rows.map((r) => {
        const show = revealed[r.id] ?? true;
        return (
          <div key={r.id} className="flex items-center gap-2" data-testid="env-row">
            <Input
              className="font-mono"
              placeholder="KEY"
              value={r.key}
              spellCheck={false}
              autoCapitalize="off"
              aria-label="Variable key"
              onChange={(e) => set(r.id, { key: e.target.value })}
            />
            <Input
              className="font-mono"
              type={show ? "text" : "password"}
              placeholder="value"
              value={r.value}
              spellCheck={false}
              autoComplete="off"
              aria-label="Variable value"
              onChange={(e) => set(r.id, { value: e.target.value })}
            />
            <Button
              type="button"
              variant="ghost"
              size="icon"
              aria-label={show ? "Hide value" : "Show value"}
              aria-pressed={!show}
              onClick={() => setRevealed((m) => ({ ...m, [r.id]: !show }))}
            >
              {show ? <EyeOff className="size-4" /> : <Eye className="size-4" />}
            </Button>
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
        );
      })}
      <Button
        type="button"
        variant="outline"
        size="sm"
        className="self-start"
        onClick={() => onChange([...rows, { id: newEnvRowId(), key: "", value: "" }])}
      >
        <Plus className="size-4" /> Add variable
      </Button>

      {dupes.length > 0 && (
        <p className="flex items-center gap-1.5 text-xs text-foreground">
          <TriangleAlert className="size-3.5 shrink-0 text-instrument-caution" />
          Duplicate {dupes.length === 1 ? "key" : "keys"} ({dupes.join(", ")}) — the last value
          wins.
        </p>
      )}

      <FieldDescription>
        Set the model here, e.g. <code className="font-mono">ANTHROPIC_MODEL</code>. There is no
        separate model field. Values are saved with the profile and injected into every session it
        launches; use the eye toggle to hide a sensitive value while editing.
      </FieldDescription>
    </div>
  );
}
