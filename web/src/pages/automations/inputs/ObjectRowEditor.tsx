/** The value half of one map row: the object fields from `valueShape`,
 * rendered inline (label + control per field). */

import { Label } from "@/components/ui/label";
import type { ValueFieldSpec } from "@/lib/automation-inputs";

import { ScalarValueEditor } from "./ScalarValueEditor";

export interface ObjectRowEditorProps {
  rowKey: string;
  fields: ValueFieldSpec[];
  value: Record<string, unknown>;
  onChange: (next: Record<string, unknown>) => void;
  errors?: Record<string, string>;
  disabled?: boolean;
}

export function ObjectRowEditor({
  rowKey,
  fields,
  value,
  onChange,
  errors,
  disabled,
}: ObjectRowEditorProps) {
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
      {fields.map((field) => {
        const error = errors?.[field.key];
        const id = `row-${rowKey}-${field.key}`;
        return (
          <div key={field.key} className="flex items-center gap-2">
            <Label htmlFor={id} className="text-muted-foreground text-xs">
              {field.label}
            </Label>
            <div id={id} className="min-w-28">
              <ScalarValueEditor
                field={field}
                value={value[field.key]}
                onChange={(next) => onChange({ ...value, [field.key]: next })}
                disabled={disabled}
                ariaLabel={`${rowKey} ${field.label}`}
              />
            </div>
            {error && (
              <span className="text-destructive text-xs" role="alert">
                {error}
              </span>
            )}
          </div>
        );
      })}
    </div>
  );
}
