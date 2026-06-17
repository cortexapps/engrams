import { Plus, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FieldDescription } from "@/components/ui/field";

export type EnvRow = { key: string; value: string };

export function envRowsToMap(rows: EnvRow[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const r of rows) if (r.key.trim()) out[r.key.trim()] = r.value;
  return out;
}
export function mapToEnvRows(map: Record<string, string>): EnvRow[] {
  return Object.entries(map).map(([key, value]) => ({ key, value }));
}

export function EnvVarsEditor({
  rows,
  onChange,
}: {
  rows: EnvRow[];
  onChange: (rows: EnvRow[]) => void;
}) {
  const set = (i: number, patch: Partial<EnvRow>) =>
    onChange(rows.map((r, idx) => (idx === i ? { ...r, ...patch } : r)));
  return (
    <div className="flex flex-col gap-2">
      {rows.map((r, i) => (
        <div key={i} className="flex items-center gap-2" data-testid="env-row">
          <Input
            className="font-mono"
            placeholder="KEY"
            value={r.key}
            spellCheck={false}
            autoCapitalize="off"
            onChange={(e) => set(i, { key: e.target.value })}
          />
          <Input
            className="font-mono"
            placeholder="value"
            value={r.value}
            spellCheck={false}
            onChange={(e) => set(i, { value: e.target.value })}
          />
          <Button
            type="button"
            variant="ghost"
            size="icon"
            aria-label="Remove"
            onClick={() => onChange(rows.filter((_, idx) => idx !== i))}
          >
            <X className="size-4" />
          </Button>
        </div>
      ))}
      <Button
        type="button"
        variant="outline"
        size="sm"
        className="self-start"
        onClick={() => onChange([...rows, { key: "", value: "" }])}
      >
        <Plus className="size-4" /> Add variable
      </Button>
      <FieldDescription>
        Set the model here, e.g. <code className="font-mono">ANTHROPIC_MODEL</code>. There is no
        separate model field.
      </FieldDescription>
    </div>
  );
}
