import { CheckIcon, MapIcon } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

/**
 * ADR 0107: the session-mode chip, shared by the start screen and the session
 * composer so both surfaces read the same.
 *
 * Mode is a BEHAVIOR switch, not a capability selector, so it keeps its own lit
 * chip on the left rather than joining the quiet model/effort group (the shape
 * Codex and the Claude desktop app both settled on). One alternate mode = a
 * toggle, because "plan this instead" is a single intent; several = a menu.
 */
export interface ModeOption {
  id: string;
  label: string;
}

interface Props {
  /** Selectable modes, excluding the harness default. Empty = render nothing. */
  modes: ModeOption[];
  /** The active mode id, or null/"default" for the harness default. */
  value: string | null;
  onChange: (next: string | null) => void;
  disabled?: boolean;
  /** Label for the default mode in the menu / the off state. */
  defaultLabel?: string;
  className?: string;
}

const chipClass = (active: boolean) =>
  cn(
    "inline-flex h-7 shrink-0 items-center gap-1 rounded-md px-2 text-xs font-medium transition-colors",
    "focus-visible:ring-[3px] focus-visible:ring-ring/50 focus-visible:outline-none",
    "disabled:pointer-events-none disabled:opacity-50",
    // The on state has to read as pressed in BOTH themes — pale lime ink on
    // the light canvas was barely legible, so it carries a ring and normal ink.
    active
      ? "bg-primary/20 text-foreground ring-1 ring-primary/40 hover:bg-primary/25"
      : "text-muted-foreground/70 hover:bg-accent hover:text-foreground",
  );

export function ModeChip({
  modes,
  value,
  onChange,
  disabled,
  defaultLabel = "Build",
  className,
}: Props) {
  const active = value != null && value !== "default";
  const current = modes.find((m) => m.id === value);
  if (modes.length === 0) return null;

  if (modes.length === 1) {
    const only = modes[0]!;
    return (
      <TooltipProvider delayDuration={200}>
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              disabled={disabled}
              aria-pressed={active}
              aria-label={`${only.label} mode`}
              data-testid="session-mode-chip"
              className={cn(chipClass(active), className)}
              onClick={() => onChange(active ? null : only.id)}
            >
              <MapIcon className="size-3.5" />
              {/* The off state is the icon alone; `aria-label` + `aria-pressed`
                  already name it, so an sr-only label would double-announce. */}
              {active && only.label}
            </button>
          </TooltipTrigger>
          <TooltipContent side="top">
            {active
              ? `${only.label} mode on — a read-only design pass`
              : `${only.label} first — the agent explores and proposes before it edits`}
          </TooltipContent>
        </Tooltip>
      </TooltipProvider>
    );
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild disabled={disabled}>
        <button
          type="button"
          aria-label="Mode"
          data-testid="session-mode-chip"
          className={cn(chipClass(active), className)}
        >
          <MapIcon className="size-3.5" />
          {current?.label ?? defaultLabel}
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="min-w-40">
        <DropdownMenuLabel className="text-muted-foreground">Mode</DropdownMenuLabel>
        <DropdownMenuItem onSelect={() => onChange(null)}>
          {defaultLabel}
          {!active && <CheckIcon className="ml-auto size-3.5" />}
        </DropdownMenuItem>
        {modes.map((m) => (
          <DropdownMenuItem key={m.id} onSelect={() => onChange(m.id)}>
            {m.label}
            {value === m.id && <CheckIcon className="ml-auto size-3.5" />}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
