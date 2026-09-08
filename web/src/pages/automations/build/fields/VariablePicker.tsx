/** Click-to-insert variable palette (ADR 0119 phase 3.3 stub).
 *
 * 3.4 extends this with groups (trigger / event / steps.<id> / inputs) and
 * live values from TestRender's per-block scope. The prop contract is fixed
 * now so that extension is additive: `paths` (static) + optional `values`
 * (path → preview) + `onInsert`. */

import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Button } from "@/components/ui/button";
import { Braces } from "lucide-react";

export interface VariablePickerProps {
  paths: readonly string[];
  /** 3.4: live preview values keyed by path. */
  values?: Readonly<Record<string, string>>;
  onInsert: (path: string) => void;
}

export function VariablePicker({ paths, values, onInsert }: VariablePickerProps) {
  if (paths.length === 0) return null;
  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button type="button" variant="ghost" size="sm" className="mt-1 self-start">
          <Braces className="size-3.5" aria-hidden />
          Insert variable
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="max-h-72 w-80 overflow-y-auto p-1">
        <ul className="text-sm" data-testid="variable-picker">
          {paths.map((path) => (
            <li key={path}>
              <button
                type="button"
                className="hover:bg-accent flex w-full items-baseline justify-between gap-3 rounded px-2 py-1 text-left font-mono text-xs"
                onClick={() => onInsert(path)}
              >
                <span>{path}</span>
                {values?.[path] !== undefined && (
                  <span className="text-muted-foreground truncate">{values[path]}</span>
                )}
              </button>
            </li>
          ))}
        </ul>
      </PopoverContent>
    </Popover>
  );
}
