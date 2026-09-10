/** One scalar input control (string / number / boolean / enum), shared by
 * the top-level form, map rows, and list elements. */

import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import type { ValueFieldSpec } from "@/lib/automation-inputs";

export interface ScalarValueEditorProps {
  field: ValueFieldSpec;
  value: unknown;
  onChange: (next: unknown) => void;
  disabled?: boolean;
  /** Accessible name; defaults to the field label. */
  ariaLabel?: string;
}

export function ScalarValueEditor({
  field,
  value,
  onChange,
  disabled,
  ariaLabel,
}: ScalarValueEditorProps) {
  const label = ariaLabel ?? field.label;
  switch (field.type) {
    case "boolean":
      return (
        <Switch
          checked={value === true}
          onCheckedChange={(checked) => onChange(checked)}
          disabled={disabled}
          aria-label={label}
        />
      );
    case "number":
      return (
        <Input
          type="number"
          inputMode="decimal"
          value={typeof value === "number" ? String(value) : ""}
          onChange={(e) => {
            const next = e.target.value;
            onChange(next === "" ? undefined : Number(next));
          }}
          disabled={disabled}
          aria-label={label}
        />
      );
    case "enum":
      return (
        <Select
          value={typeof value === "string" ? value : ""}
          onValueChange={(next) => onChange(next)}
          disabled={disabled}
        >
          <SelectTrigger aria-label={label}>
            <SelectValue placeholder="Choose…" />
          </SelectTrigger>
          <SelectContent>
            {(field.values ?? []).map((option) => (
              <SelectItem key={option} value={option}>
                {option.replaceAll("_", " ")}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      );
    default:
      return (
        <Input
          value={typeof value === "string" ? value : ""}
          onChange={(e) => onChange(e.target.value)}
          disabled={disabled}
          aria-label={label}
        />
      );
  }
}
