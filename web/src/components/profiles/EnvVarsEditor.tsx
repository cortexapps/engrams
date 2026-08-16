import { useRef, useState } from "react";
import { Eye, EyeOff, Plus, TriangleAlert, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FieldDescription } from "@/components/ui/field";

export type EnvRow = { id: string; key: string; value: string };

/** Just enough of a profile app (ADR 0118) to name its injected variables. */
export type EnvAppRef = { name: string; port: number };

/**
 * The env-var stem for an app: uppercased, every run of non-alphanumerics
 * collapsed to one `_`. Mirrors `envStem` in the orchestrator's apps/env.ts —
 * the two must agree or the editor advertises names the platform never injects.
 */
export function envStem(appName: string): string {
  return appName.toUpperCase().replace(/[^A-Z0-9]+/g, "_");
}

/**
 * `${…_INGRESS_HOST}` / `${…_INGRESS_URL}` references inside a value.
 *
 * Deliberately narrower than the orchestrator's general `${NAME}` interpolation:
 * an unknown reference of ANY other shape is left verbatim on purpose (values
 * legitimately carry shell syntax), so flagging those would be wrong. An
 * ingress-shaped one that resolves to nothing is always a typo or a renamed
 * app — which is exactly the set `interpolateEnv` reports as unresolved.
 */
const INGRESS_REF_RE = /\$\{([A-Za-z_][A-Za-z0-9_]*_INGRESS_(?:HOST|URL))\}/g;

function schemeFor(domain: string): "http" | "https" {
  return /(localhost|127\.0\.0\.1|lvh\.me|localtest\.me)/.test(domain) ? "http" : "https";
}

/** The two variables an app contributes, in the order they are offered. */
function appVars(app: EnvAppRef): { token: string; suffix: "URL" | "HOST" }[] {
  const stem = envStem(app.name);
  return [
    { token: `\${${stem}_INGRESS_URL}`, suffix: "URL" },
    { token: `\${${stem}_INGRESS_HOST}`, suffix: "HOST" },
  ];
}

/**
 * What a value will look like once the platform substitutes it, with
 * `<session>` standing in for the per-session slug. Returns null when the value
 * carries no resolvable reference, so the caller renders nothing.
 */
export function previewResolved(
  value: string,
  apps: readonly EnvAppRef[],
  baseDomain: string | undefined,
): string | null {
  if (!baseDomain) return null;
  let hit = false;
  const out = value.replace(INGRESS_REF_RE, (whole, name: string) => {
    const app = apps.find((a) => `${envStem(a.name)}_INGRESS_URL` === name);
    if (app) {
      hit = true;
      return `${schemeFor(baseDomain)}://${app.name}-<session>.${baseDomain}`;
    }
    const hostApp = apps.find((a) => `${envStem(a.name)}_INGRESS_HOST` === name);
    if (hostApp) {
      hit = true;
      return `${hostApp.name}-<session>.${baseDomain}`;
    }
    return whole;
  });
  return hit ? out : null;
}

/** Ingress-shaped references across every value that name no declared app. */
export function danglingRefs(rows: readonly EnvRow[], apps: readonly EnvAppRef[]): string[] {
  const known = new Set(apps.flatMap((a) => appVars(a).map((v) => v.token)));
  const out = new Set<string>();
  for (const r of rows) {
    for (const m of r.value.matchAll(INGRESS_REF_RE)) {
      if (!known.has(m[0])) out.add(m[0]);
    }
  }
  return [...out];
}

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
  apps = [],
  previewBaseDomain,
}: {
  rows: EnvRow[];
  onChange: (rows: EnvRow[]) => void;
  /** Declared apps, whose addresses become injectable variables. */
  apps?: readonly EnvAppRef[];
  /** Deployment preview domain, for the resolved-shape hint. Optional. */
  previewBaseDomain?: string;
}) {
  // Values default to visible (most are config like ANTHROPIC_MODEL); the eye
  // toggle hides a sensitive one while editing, so a secret isn't left in the
  // clear on screen.
  const [revealed, setRevealed] = useState<Record<string, boolean>>({});
  const set = (id: string, patch: Partial<EnvRow>) =>
    onChange(rows.map((r) => (r.id === id ? { ...r, ...patch } : r)));

  // Insert a variable at the caret of the value the admin was last editing.
  // "Last focused" rather than "currently focused" on purpose: it stays useful
  // after a click lands elsewhere, and the chips suppress their own mousedown
  // so the caret never moves in the first place.
  const valueRefs = useRef(new Map<string, HTMLInputElement>());
  const [lastFocusedId, setLastFocusedId] = useState<string | null>(null);
  const insertToken = (token: string) => {
    const el = lastFocusedId ? valueRefs.current.get(lastFocusedId) : undefined;
    if (!lastFocusedId || !el) {
      // Nothing to insert into yet — start a row holding it, so the click is
      // never a no-op the admin has to puzzle over.
      onChange([...rows, { id: newEnvRowId(), key: "", value: token }]);
      return;
    }
    const start = el.selectionStart ?? el.value.length;
    const end = el.selectionEnd ?? start;
    set(lastFocusedId, { value: el.value.slice(0, start) + token + el.value.slice(end) });
    const caret = start + token.length;
    requestAnimationFrame(() => {
      el.focus();
      el.setSelectionRange(caret, caret);
    });
  };

  const dangling = danglingRefs(rows, apps);

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
        const resolved = show ? previewResolved(r.value, apps, previewBaseDomain) : null;
        return (
          <div key={r.id} data-testid="env-row">
            <div className="flex items-center gap-2">
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
                ref={(el) => {
                  if (el) valueRefs.current.set(r.id, el);
                  else valueRefs.current.delete(r.id);
                }}
                className="font-mono"
                type={show ? "text" : "password"}
                placeholder="value"
                value={r.value}
                spellCheck={false}
                autoComplete="off"
                aria-label="Variable value"
                onFocus={() => setLastFocusedId(r.id)}
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
            {resolved && (
              <p
                className="mt-1 ml-1 font-mono text-[0.7rem] break-all text-muted-foreground"
                data-testid="env-resolved-preview"
              >
                → {resolved}
              </p>
            )}
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

      {dangling.length > 0 && (
        <p className="flex items-start gap-1.5 text-xs text-foreground" data-testid="env-dangling">
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-instrument-caution" />
          <span>
            <code className="font-mono">{dangling.join(", ")}</code>{" "}
            {dangling.length === 1 ? "names" : "name"} no app — declare it under Apps, or fix the
            name. The platform leaves an unknown reference in the value as-is.
          </span>
        </p>
      )}

      {/* The injectable addresses, derived live from the declared apps (ADR
          0118). This is the only place the names are discoverable: they exist
          because of the Apps list and change when an app is renamed, so static
          help text would go stale. */}
      {apps.length > 0 && (
        <div className="rounded-md border bg-muted/30 px-2.5 py-2" data-testid="env-app-vars">
          <p className="text-[0.7rem] text-muted-foreground">
            From your apps — click to insert. <code className="font-mono">_URL</code> carries the
            scheme (base URLs); <code className="font-mono">_HOST</code> is the bare hostname (CORS
            allow-lists, cookie domains).
          </p>
          <div className="mt-1.5 flex flex-col gap-1">
            {apps.map((a) => (
              <div key={a.name} className="flex flex-wrap items-center gap-1.5">
                <span className="w-20 shrink-0 truncate font-mono text-[0.7rem] text-muted-foreground">
                  {a.name}
                </span>
                {appVars(a).map((v) => (
                  <button
                    key={v.token}
                    type="button"
                    data-testid={`env-var-chip-${envStem(a.name)}_INGRESS_${v.suffix}`}
                    // Keep the caret where it is — a focused input must not blur
                    // when the chip is pressed, or there is nowhere to insert.
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => insertToken(v.token)}
                    className="rounded-md border bg-background px-1.5 py-0.5 font-mono text-[0.7rem] hover:bg-accent"
                  >
                    {v.token}
                  </button>
                ))}
              </div>
            ))}
          </div>
        </div>
      )}

      <FieldDescription>
        Extra env vars injected into every session this profile launches. Model and effort have
        dedicated controls above — set those there, not here. Values are saved with the profile; use
        the eye toggle to hide a sensitive value while editing.
        {apps.length === 0 && " Declare an app above to reference its public address here."}
      </FieldDescription>
    </div>
  );
}
