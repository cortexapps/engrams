import { CheckIcon, ChevronDownIcon } from "lucide-react";
import { useMemo } from "react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";
import type { HarnessOption, HarnessSummary } from "../../gen/engram/app/v1/harness_pb";

/** The per-session harness/model/effort override (ADR 0063 B2). `null` on a
 *  field = inherit the profile's default (and, failing that, the descriptor
 *  default — resolved server-side). The composer sends only the non-null ones. */
export interface HarnessOverride {
  harness: string | null;
  model: string | null;
  effort: string | null;
  /** ADR 0107: session mode for the initial prompt ("plan"); null = default. */
  mode: string | null;
}

export const EMPTY_OVERRIDE: HarnessOverride = {
  harness: null,
  model: null,
  effort: null,
  mode: null,
};

interface Props {
  /** The registered harness catalog (undefined while loading). */
  harnesses: HarnessSummary[] | undefined;
  /** The selected profile's default harness (catalog name), or undefined. */
  profileHarness?: string;
  value: HarnessOverride;
  onChange: (next: HarnessOverride) => void;
  disabled?: boolean;
}

/** One quiet text control: the resolved value, a menu to change it. */
function OptionMenu({
  heading,
  current,
  inheritLabel,
  options,
  selected,
  onSelect,
  disabled,
  testId,
}: {
  heading: string;
  /** What the launch will actually use — shown on the trigger. */
  current: string;
  /** The "no override" row's label, e.g. "Profile default". */
  inheritLabel: string;
  options: { id: string; label: string }[];
  selected: string | null;
  onSelect: (id: string | null) => void;
  disabled?: boolean;
  testId: string;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild disabled={disabled}>
        <button
          type="button"
          aria-label={heading}
          data-testid={testId}
          className={cn(
            "inline-flex h-7 max-w-40 items-center gap-1 rounded-md px-1.5 text-xs font-medium",
            "text-muted-foreground transition-colors hover:bg-accent hover:text-foreground",
            "focus-visible:ring-[3px] focus-visible:ring-ring/50 focus-visible:outline-none",
            "disabled:pointer-events-none disabled:opacity-50",
            selected != null && "text-foreground",
          )}
        >
          <span className="truncate">{current}</span>
          <ChevronDownIcon className="size-3 shrink-0 opacity-40" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="min-w-44">
        <DropdownMenuLabel className="text-muted-foreground">{heading}</DropdownMenuLabel>
        <DropdownMenuItem onSelect={() => onSelect(null)}>
          {inheritLabel}
          {selected == null && <CheckIcon className="ml-auto size-3.5" />}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        {options.map((option) => (
          <DropdownMenuItem key={option.id} onSelect={() => onSelect(option.id)}>
            {option.label}
            {selected === option.id && <CheckIcon className="ml-auto size-3.5" />}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

const optionLabel = (options: HarnessOption[], id: string | null): string | undefined =>
  options.find((o) => o.id === id)?.label || (id ?? undefined);

/** The label a launch resolves to with no override: the descriptor's default. */
const defaultLabel = (options: HarnessOption[]): string | undefined => {
  const fallback = options.find((o) => o.default);
  return fallback ? fallback.label || fallback.id : undefined;
};

/**
 * Compact harness / model / effort pickers for the launch composer. A profile
 * carries defaults; here a session overrides them (e.g. Claude Code for planning,
 * a smaller model for execution — ADR 0063 §B2). The model + effort options are
 * enums on the *effective* harness's descriptor, so changing the harness clears a
 * now-stale model/effort. The harness picker only appears when more than one
 * harness is registered (nothing to choose otherwise).
 *
 * These are CAPABILITY selectors, so they read as quiet text ("Codex · Opus ·
 * High") rather than four filled pills — the shape Codex and the Claude desktop
 * app converged on. Each trigger shows the value the launch will actually use,
 * not the word "Default", so the row states the truth at a glance. Mode is not
 * here: it is a behavior switch and keeps its own lit chip (`ModeChip`).
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
  const models = descriptor?.models ?? [];
  const effort = descriptor?.effort ?? [];
  if (!showHarness && models.length === 0 && effort.length === 0) return null;

  const harnessLabel = (name: string | undefined) =>
    harnesses?.find((h) => h.name === name)?.descriptor?.label || name;

  return (
    <div className="flex min-w-0 items-center gap-0.5" data-testid="session-harness-controls">
      {showHarness && (
        <OptionMenu
          heading="Harness"
          testId="session-harness-select"
          current={harnessLabel(effectiveHarness) ?? "Harness"}
          inheritLabel="Profile harness"
          options={(harnesses ?? []).map((h) => ({
            id: h.name,
            label: h.descriptor?.label || h.name,
          }))}
          selected={value.harness}
          disabled={disabled}
          // Harness changed → model/effort enums belong to it; clear the old ones.
          onSelect={(id) => onChange({ harness: id, model: null, effort: null, mode: value.mode })}
        />
      )}
      {models.length > 0 && (
        <OptionMenu
          heading="Model"
          testId="session-model-select"
          current={optionLabel(models, value.model) ?? defaultLabel(models) ?? "Model"}
          inheritLabel="Default model"
          options={models.map((m) => ({ id: m.id, label: m.label || m.id }))}
          selected={value.model}
          disabled={disabled}
          onSelect={(model) => onChange({ ...value, model })}
        />
      )}
      {effort.length > 0 && (
        <OptionMenu
          heading="Effort"
          testId="session-effort-select"
          current={optionLabel(effort, value.effort) ?? defaultLabel(effort) ?? "Effort"}
          inheritLabel="Default effort"
          options={effort.map((e) => ({ id: e.id, label: e.label || e.id }))}
          selected={value.effort}
          disabled={disabled}
          onSelect={(next) => onChange({ ...value, effort: next })}
        />
      )}
    </div>
  );
}
