/** A `secret_ref` input: pick an existing org-secret NAME (values are never
 * shown; the orchestrator resolves the ref at run time). Reuses the same
 * metadata list the Secrets settings page and the profile picker use. */

import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useOrgSecretNames } from "@/hooks/useOrgSecrets";

export interface SecretRefPickerProps {
  value: string;
  onChange: (next: string) => void;
  disabled?: boolean;
  ariaLabel: string;
}

export function SecretRefPicker({ value, onChange, disabled, ariaLabel }: SecretRefPickerProps) {
  const names = useOrgSecretNames();
  const options = names.data ?? [];
  // Keep a stored ref selectable even if the secret was since deleted, so
  // the user sees (and can change) what is configured.
  const all = value && !options.includes(value) ? [value, ...options] : options;
  return (
    <Select value={value} onValueChange={onChange} disabled={disabled || names.isLoading}>
      <SelectTrigger aria-label={ariaLabel}>
        <SelectValue placeholder={names.isLoading ? "Loading secrets…" : "Choose a secret…"} />
      </SelectTrigger>
      <SelectContent>
        {all.map((name) => (
          <SelectItem key={name} value={name}>
            {name}
            {!options.includes(name) && (
              <span className="text-muted-foreground ml-2 text-xs">(missing)</span>
            )}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
