import { CheckIcon, ChevronDownIcon } from "lucide-react";
import { useMemo } from "react";
import { RouterModelAudience } from "../../gen/engram/app/v1/model_router_pb";
import { useModelRouters, useRouterModels } from "@/hooks/useModelRouters";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
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

/** The per-session harness/router/model/effort override (ADR 0063 B2, ADR 0117). `null` on a
 *  field = inherit the profile's default (and, failing that, the descriptor
 *  default — resolved server-side). The composer sends only the non-null ones. */
export interface HarnessOverride {
  harness: string | null;
  model: string | null;
  /** null = inherit profile, empty string = direct/native, id = routed. */
  modelRouter?: string | null;
  effort: string | null;
  /** ADR 0107: session mode for the initial prompt ("plan"); null = default. */
  mode: string | null;
}

export const EMPTY_OVERRIDE: HarnessOverride = {
  harness: null,
  model: null,
  modelRouter: null,
  effort: null,
  mode: null,
};

interface Props {
  /** The registered harness catalog (undefined while loading). */
  harnesses: HarnessSummary[] | undefined;
  /** The selected profile's default harness (catalog name), or undefined. */
  profileHarness?: string;
  profileModelRouter?: string;
  profileModel?: string;
  value: HarnessOverride;
  onChange: (next: HarnessOverride) => void;
  disabled?: boolean;
  audience?: "user" | "programmatic";
}

export function SearchableOptionMenu({
  current,
  options,
  selected,
  onSelect,
  disabled,
  inheritLabel = "Profile default",
  testId = "session-model-select",
  label = "Model",
}: {
  current: string;
  options: Array<{ id: string; label: string; detail?: string }>;
  selected: string | null;
  onSelect: (id: string | null) => void;
  disabled?: boolean;
  inheritLabel?: string;
  testId?: string;
  label?: string;
}) {
  return (
    <Popover>
      <PopoverTrigger asChild disabled={disabled}>
        <button
          type="button"
          aria-label={label}
          data-testid={testId}
          className={cn(
            "inline-flex h-7 max-w-56 items-center gap-1 rounded-md px-1.5 text-xs font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-[3px] focus-visible:ring-ring/50 focus-visible:outline-none",
            selected != null && "text-foreground",
          )}
        >
          <span className="truncate">{current}</span>
          <ChevronDownIcon className="size-3 shrink-0 opacity-40" />
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-[24rem] p-0">
        <Command>
          <CommandInput placeholder="Search models…" />
          <CommandList>
            <CommandEmpty>No models found.</CommandEmpty>
            <CommandGroup>
              <CommandItem value={inheritLabel} onSelect={() => onSelect(null)}>
                {inheritLabel}
                {selected == null && <CheckIcon className="ml-auto size-3.5" />}
              </CommandItem>
              {options.map((option) => (
                <CommandItem
                  key={option.id}
                  value={`${option.label} ${option.id} ${option.detail ?? ""}`}
                  onSelect={() => onSelect(option.id)}
                >
                  <div className="min-w-0">
                    <div className="truncate">{option.label}</div>
                    {option.detail && (
                      <div className="truncate font-mono text-[11px] text-muted-foreground">
                        {option.detail}
                      </div>
                    )}
                  </div>
                  {selected === option.id && <CheckIcon className="ml-auto size-3.5" />}
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
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
  profileModelRouter,
  profileModel,
  value,
  onChange,
  disabled,
  audience = "user",
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
  const routersQuery = useModelRouters();
  const routers = routersQuery.data?.routers ?? [];
  const compatibleRouters = routers.filter((router) =>
    descriptor?.routerProtocols?.some((protocol) => router.protocols.includes(protocol)),
  );
  const effectiveRouter =
    value.modelRouter === null ? profileModelRouter : value.modelRouter || undefined;
  const effectiveRouterDefinition = routers.find((router) => router.id === effectiveRouter);
  const routerModelsQuery = useRouterModels(
    effectiveRouter ?? "",
    "",
    audience === "programmatic" ? RouterModelAudience.PROGRAMMATIC : RouterModelAudience.USER,
  );
  const routerModels = routerModelsQuery.data?.models ?? [];

  const showHarness = (harnesses?.length ?? 0) > 1;
  const models = effectiveRouter ? [] : (descriptor?.models ?? []);
  const effort = descriptor?.effort ?? [];
  const effectiveRoutedModelId =
    value.model ?? profileModel ?? effectiveRouterDefinition?.defaultModel;
  const selectedRoutedModel = routerModels.find((model) => model.id === effectiveRoutedModelId);
  const showEffort = !effectiveRouter || selectedRoutedModel?.supportsReasoning !== false;
  if (!showHarness && models.length === 0 && routerModels.length === 0 && effort.length === 0)
    return null;

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
          onSelect={(id) => {
            const next = harnesses?.find((h) => h.name === (id ?? profileHarness))?.descriptor;
            const router = routers.find((item) => item.id === effectiveRouter);
            const compatible = Boolean(
              router &&
              next?.routerProtocols?.some((protocol) => router.protocols.includes(protocol)),
            );
            onChange({
              harness: id,
              modelRouter: compatible ? value.modelRouter : null,
              model: compatible ? value.model : null,
              effort: compatible ? value.effort : null,
              mode: value.mode,
            });
          }}
        />
      )}
      <OptionMenu
        heading="Route"
        testId="session-router-select"
        current={
          effectiveRouter
            ? (routers.find((router) => router.id === effectiveRouter)?.label ?? effectiveRouter)
            : "Direct"
        }
        inheritLabel="Profile route"
        options={[
          { id: "", label: "Direct" },
          ...compatibleRouters.map((router) => ({ id: router.id, label: router.label })),
        ]}
        selected={value.modelRouter ?? null}
        disabled={disabled}
        onSelect={(modelRouter) => onChange({ ...value, modelRouter, model: null })}
      />
      {(models.length > 0 || routerModels.length > 0) && (
        <SearchableOptionMenu
          current={
            effectiveRouter
              ? (routerModels.find((m) => m.id === effectiveRoutedModelId)?.name ??
                effectiveRoutedModelId ??
                "Model")
              : (optionLabel(models, value.model) ?? defaultLabel(models) ?? "Model")
          }
          options={
            effectiveRouter
              ? routerModels.map((model) => ({ id: model.id, label: model.name, detail: model.id }))
              : models.map((model) => ({ id: model.id, label: model.label || model.id }))
          }
          selected={value.model}
          disabled={disabled}
          onSelect={(model) => onChange({ ...value, model })}
        />
      )}
      {showEffort && effort.length > 0 && (
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
