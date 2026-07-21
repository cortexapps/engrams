import { useMemo } from "react";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { HarnessSummary } from "../../gen/engram/app/v1/harness_pb";

/** The per-session harness/model/effort override (ADR 0063 B2). `null` on a
 *  field = inherit the profile's default (and, failing that, the descriptor
 *  default — resolved server-side). The composer sends only the non-null ones. */
export interface HarnessOverride {
  harness: string | null;
  model: string | null;
  effort: string | null;
}

export const EMPTY_OVERRIDE: HarnessOverride = { harness: null, model: null, effort: null };

const INHERIT = "__inherit__";

interface Props {
  /** The registered harness catalog (undefined while loading). */
  harnesses: HarnessSummary[] | undefined;
  /** The selected profile's default harness (catalog name), or undefined. */
  profileHarness?: string;
  value: HarnessOverride;
  onChange: (next: HarnessOverride) => void;
  disabled?: boolean;
}

/**
 * Compact harness / model / effort pickers for the launch composer. A profile
 * carries defaults; here a session overrides them (e.g. Claude Code for planning,
 * a smaller model for execution — ADR 0063 §B2). The model + effort options are
 * enums on the *effective* harness's descriptor, so changing the harness clears a
 * now-stale model/effort. The harness picker only appears when more than one
 * harness is registered (nothing to choose otherwise).
 */
export function SessionHarnessControls({
  harnesses,
  profileHarness,
  value,
  onChange,
  disabled,
}: Props) {
  // The effective harness whose descriptor drives the model/effort lists:
  // override → profile default → the sole registered harness.
  const effectiveHarness =
    value.harness ??
    profileHarness ??
    (harnesses && harnesses.length === 1 ? harnesses[0]!.name : undefined);
  const descriptor = useMemo(
    () => harnesses?.find((h) => h.name === effectiveHarness)?.descriptor,
    [harnesses, effectiveHarness],
  );

  const showHarness = (harnesses?.length ?? 0) > 1;
  const hasModels = (descriptor?.models.length ?? 0) > 0;
  const hasEffort = (descriptor?.effort.length ?? 0) > 0;
  if (!showHarness && !hasModels && !hasEffort) return null;

  const triggerClass =
    "h-7 w-auto gap-1 rounded-md border-0 bg-accent/60 px-2 text-xs font-medium text-muted-foreground hover:bg-accent focus:ring-1";

  return (
    <div className="flex flex-wrap items-center gap-1.5" data-testid="session-harness-controls">
      {showHarness && (
        <Select
          value={value.harness ?? INHERIT}
          disabled={disabled}
          onValueChange={(v) =>
            // Harness changed → model/effort enums belong to it; clear the old ones.
            onChange({ harness: v === INHERIT ? null : v, model: null, effort: null })
          }
        >
          <SelectTrigger
            aria-label="Harness"
            data-testid="session-harness-select"
            className={triggerClass}
          >
            <SelectValue placeholder="Harness" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={INHERIT}>Profile harness</SelectItem>
            {(harnesses ?? []).map((h) => (
              <SelectItem key={h.name} value={h.name}>
                {h.descriptor?.label || h.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      )}

      {hasModels && (
        <Select
          value={value.model ?? INHERIT}
          disabled={disabled}
          onValueChange={(v) => onChange({ ...value, model: v === INHERIT ? null : v })}
        >
          <SelectTrigger
            aria-label="Model"
            data-testid="session-model-select"
            className={triggerClass}
          >
            <SelectValue placeholder="Model" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={INHERIT}>Default model</SelectItem>
            {(descriptor?.models ?? []).map((m) => (
              <SelectItem key={m.id} value={m.id}>
                {m.label || m.id}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      )}

      {hasEffort && (
        <Select
          value={value.effort ?? INHERIT}
          disabled={disabled}
          onValueChange={(v) => onChange({ ...value, effort: v === INHERIT ? null : v })}
        >
          <SelectTrigger
            aria-label="Effort"
            data-testid="session-effort-select"
            className={triggerClass}
          >
            <SelectValue placeholder="Effort" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={INHERIT}>Default effort</SelectItem>
            {(descriptor?.effort ?? []).map((e) => (
              <SelectItem key={e.id} value={e.id}>
                {e.label || e.id}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      )}
    </div>
  );
}
