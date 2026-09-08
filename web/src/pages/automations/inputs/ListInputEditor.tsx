/** A list input: ordered scalar elements (the element type from
 * `valueShape.element`, default string). Enum-typed lists render as a
 * checklist — e.g. the review built-in's `categories` pick from six lenses. */

import { Plus, Trash2 } from "lucide-react";
import { useMemo } from "react";

import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  listElementField,
  type InputFieldError,
  type InputFieldSpec,
} from "@/lib/automation-inputs";

import { ScalarValueEditor } from "./ScalarValueEditor";

export interface ListInputEditorProps {
  spec: InputFieldSpec;
  value: unknown[];
  onChange: (next: unknown[]) => void;
  errors?: InputFieldError[];
  disabled?: boolean;
}

export function ListInputEditor({ spec, value, onChange, errors, disabled }: ListInputEditorProps) {
  const element = useMemo(() => listElementField(spec), [spec]);
  const errorByIndex = useMemo(() => {
    const out = new Map<number, string>();
    for (const err of errors ?? []) {
      if (err.path === undefined) continue;
      const i = Number(err.path);
      if (Number.isInteger(i)) out.set(i, err.message);
    }
    return out;
  }, [errors]);

  if (element.type === "enum") {
    const chosen = new Set(value.filter((v): v is string => typeof v === "string"));
    return (
      <ul className="flex flex-col gap-2" data-testid={`list-input-${spec.key}`}>
        {(element.values ?? []).map((option) => {
          const id = `${spec.key}-${option}`;
          return (
            <li key={option} className="flex items-center gap-3">
              <Switch
                id={id}
                checked={chosen.has(option)}
                onCheckedChange={(on) => {
                  const next = (element.values ?? []).filter((o) =>
                    o === option ? on : chosen.has(o),
                  );
                  onChange(next);
                }}
                disabled={disabled}
                aria-label={option}
              />
              <Label htmlFor={id} className="font-normal">
                {option.replaceAll("_", " ")}
              </Label>
            </li>
          );
        })}
      </ul>
    );
  }

  return (
    <div className="flex flex-col gap-2" data-testid={`list-input-${spec.key}`}>
      {value.map((item, i) => (
        <div key={i} className="flex items-center gap-2">
          <div className="flex-1">
            <ScalarValueEditor
              field={element}
              value={item}
              onChange={(next) => onChange(value.map((v, j) => (j === i ? next : v)))}
              disabled={disabled}
              ariaLabel={`${spec.label} ${i + 1}`}
            />
          </div>
          {errorByIndex.get(i) && (
            <span className="text-destructive text-xs" role="alert">
              {errorByIndex.get(i)}
            </span>
          )}
          <Button
            type="button"
            variant="ghost"
            size="icon"
            onClick={() => onChange(value.filter((_, j) => j !== i))}
            disabled={disabled}
            aria-label={`Remove ${spec.label} ${i + 1}`}
          >
            <Trash2 className="size-4" aria-hidden />
          </Button>
        </div>
      ))}
      <Button
        type="button"
        variant="outline"
        size="sm"
        className="self-start"
        onClick={() =>
          onChange([
            ...value,
            element.type === "number" ? 0 : element.type === "boolean" ? false : "",
          ])
        }
        disabled={disabled}
      >
        <Plus className="size-4" aria-hidden />
        Add
      </Button>
    </div>
  );
}
