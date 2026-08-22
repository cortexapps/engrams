/** Structured condition rows (field · op · value) with all/any groups —
 * the editor half of the engine's one condition evaluator
 * (orchestrator/src/automations/engine/conditions.ts). Operators and the
 * depth/leaf caps mirror that module. */

import { Plus, Trash2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

export const FILTER_OPERATORS = [
  "equals",
  "not_equals",
  "contains",
  "matches",
  "in",
  "is_empty",
  "gt",
  "lt",
  "is_true",
  "is_false",
] as const;
export type FilterOperator = (typeof FILTER_OPERATORS)[number];

const VALUELESS: ReadonlySet<string> = new Set(["is_empty", "is_true", "is_false"]);
const MAX_DEPTH = 3;

export interface FilterCondition {
  path: string;
  op: FilterOperator;
  value?: unknown;
}
export interface FilterGroup {
  mode: "all" | "any";
  conditions: Array<FilterCondition | FilterGroup>;
}

export const EMPTY_GROUP: FilterGroup = { mode: "all", conditions: [] };

export function isGroup(node: FilterCondition | FilterGroup): node is FilterGroup {
  return "mode" in node;
}

export function asFilterGroup(value: unknown): FilterGroup {
  if (typeof value === "object" && value !== null && "mode" in value) return value as FilterGroup;
  return EMPTY_GROUP;
}

/** A value typed into the row: JSON when it parses, else the raw string —
 * so `42`, `true`, `["a","b"]` round-trip as their JSON types. */
function parseValue(raw: string): unknown {
  if (raw === "") return undefined;
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

function valueText(value: unknown): string {
  if (value === undefined) return "";
  if (typeof value === "string") return value;
  return JSON.stringify(value);
}

interface Props {
  value: FilterGroup;
  onChange: (next: FilterGroup) => void;
  disabled?: boolean;
  /** Static path suggestions. */
  paths?: readonly string[];
  depth?: number;
}

export function ConditionEditor({ value, onChange, disabled, paths, depth = 1 }: Props) {
  const update = (index: number, node: FilterCondition | FilterGroup) => {
    const conditions = [...value.conditions];
    conditions[index] = node;
    onChange({ ...value, conditions });
  };
  const remove = (index: number) => {
    onChange({ ...value, conditions: value.conditions.filter((_, i) => i !== index) });
  };
  const listId = `cond-paths-${depth}`;

  return (
    <div className="space-y-2 rounded-md border p-2" data-testid={`condition-group-${depth}`}>
      <div className="flex items-center gap-2 text-sm">
        <span className="text-muted-foreground">Match</span>
        <Select
          value={value.mode}
          onValueChange={(mode) => onChange({ ...value, mode: mode as "all" | "any" })}
          disabled={disabled}
        >
          <SelectTrigger className="h-8 w-24">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">all</SelectItem>
            <SelectItem value="any">any</SelectItem>
          </SelectContent>
        </Select>
        <span className="text-muted-foreground">of the following</span>
      </div>

      {paths && paths.length > 0 && (
        <datalist id={listId}>
          {paths.map((p) => (
            <option key={p} value={p} />
          ))}
        </datalist>
      )}

      {value.conditions.map((node, index) =>
        isGroup(node) ? (
          <div key={index} className="flex items-start gap-1">
            <div className="flex-1">
              <ConditionEditor
                value={node}
                onChange={(next) => update(index, next)}
                disabled={disabled}
                paths={paths}
                depth={depth + 1}
              />
            </div>
            {!disabled && (
              <Button
                type="button"
                variant="ghost"
                size="icon"
                aria-label="Remove group"
                onClick={() => remove(index)}
              >
                <Trash2 className="size-4" />
              </Button>
            )}
          </div>
        ) : (
          <div key={index} className="flex items-center gap-1" data-testid="condition-row">
            <Input
              className="flex-1 font-mono text-xs"
              placeholder="event.pr.draft"
              list={listId}
              value={node.path}
              onChange={(e) => update(index, { ...node, path: e.target.value })}
              disabled={disabled}
            />
            <Select
              value={node.op}
              onValueChange={(op) => update(index, { ...node, op: op as FilterOperator })}
              disabled={disabled}
            >
              <SelectTrigger className="h-8 w-32">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {FILTER_OPERATORS.map((op) => (
                  <SelectItem key={op} value={op}>
                    {op.replaceAll("_", " ")}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {!VALUELESS.has(node.op) && (
              <Input
                className="flex-1 font-mono text-xs"
                placeholder={node.op === "in" ? '["a","b"]' : "value"}
                value={valueText(node.value)}
                onChange={(e) => update(index, { ...node, value: parseValue(e.target.value) })}
                disabled={disabled}
              />
            )}
            {!disabled && (
              <Button
                type="button"
                variant="ghost"
                size="icon"
                aria-label="Remove condition"
                onClick={() => remove(index)}
              >
                <Trash2 className="size-4" />
              </Button>
            )}
          </div>
        ),
      )}

      {!disabled && (
        <div className="flex gap-1">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() =>
              onChange({
                ...value,
                conditions: [...value.conditions, { path: "", op: "equals", value: "" }],
              })
            }
          >
            <Plus className="size-3.5" aria-hidden /> Condition
          </Button>
          {depth < MAX_DEPTH && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={() =>
                onChange({
                  ...value,
                  conditions: [...value.conditions, { mode: "any", conditions: [] }],
                })
              }
            >
              <Plus className="size-3.5" aria-hidden /> Group
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
