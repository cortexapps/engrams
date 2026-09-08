/** A map input keyed by an integration noun (repository / channel / team).
 *
 * Rows are `key → object` per the schema's `valueShape`. New keys come from
 * the noun picker (ListInputKeyOptions — ledger-observed, so it lists only
 * what has already delivered an event) OR a free-typed key: a repository
 * must be addable before its first event arrives, otherwise the review
 * built-in could never be scoped to a fresh repo. */

import { Check, Plus, Trash2 } from "lucide-react";
import { useMemo, useState } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useInputKeyOptions } from "@/hooks/useAutomationInputs";
import {
  defaultMapRow,
  isValidMapKey,
  mapKeyHint,
  mapValueFields,
  normalizeMapKey,
  type InputFieldError,
  type InputFieldSpec,
} from "@/lib/automation-inputs";

import { ObjectRowEditor } from "./ObjectRowEditor";

export interface MapInputEditorProps {
  spec: InputFieldSpec;
  value: Record<string, unknown>;
  onChange: (next: Record<string, unknown>) => void;
  errors?: InputFieldError[];
  disabled?: boolean;
}

export function MapInputEditor({ spec, value, onChange, errors, disabled }: MapInputEditorProps) {
  const fields = useMemo(() => mapValueFields(spec), [spec]);
  const options = useInputKeyOptions(spec.keyNoun);
  const [draftKey, setDraftKey] = useState("");
  const [keyError, setKeyError] = useState<string | null>(null);

  const rows = Object.entries(value);
  const existing = new Set(Object.keys(value));
  const unusedOptions = (options.data?.options ?? []).filter((o) => !existing.has(o.key));

  // Per-row error index: "<rowKey>" → row-level, "<rowKey>.<field>" → field.
  const rowErrors = useMemo(() => {
    const byRow = new Map<string, { row?: string; fields: Record<string, string> }>();
    for (const err of errors ?? []) {
      if (!err.path) continue;
      const [rowKey, field] = splitPath(err.path);
      const entry = byRow.get(rowKey) ?? { fields: {} };
      if (field) entry.fields[field] = err.message;
      else entry.row = err.message;
      byRow.set(rowKey, entry);
    }
    return byRow;
  }, [errors]);

  const addKey = (raw: string) => {
    const key = normalizeMapKey(spec.keyNoun, raw);
    if (!isValidMapKey(spec.keyNoun, key)) {
      setKeyError(`Enter a ${mapKeyHint(spec.keyNoun)}`);
      return;
    }
    if (existing.has(key)) {
      setKeyError("Already listed");
      return;
    }
    setKeyError(null);
    setDraftKey("");
    onChange({ ...value, [key]: defaultMapRow(spec) });
  };

  const removeKey = (key: string) => {
    const next = { ...value };
    delete next[key];
    onChange(next);
  };

  return (
    <div className="flex flex-col gap-3" data-testid={`map-input-${spec.key}`}>
      {rows.length === 0 && <p className="text-muted-foreground text-sm">Nothing listed yet.</p>}
      <ul className="flex flex-col gap-2">
        {rows.map(([rowKey, row]) => {
          const errs = rowErrors.get(rowKey);
          const rowValue =
            typeof row === "object" && row !== null && !Array.isArray(row)
              ? (row as Record<string, unknown>)
              : {};
          return (
            <li
              key={rowKey}
              className="bg-card flex flex-wrap items-center gap-3 rounded-lg border px-3 py-2"
              data-testid={`map-row-${rowKey}`}
            >
              <code className="text-sm font-medium">{rowKey}</code>
              {errs?.row && (
                <span className="text-destructive text-xs" role="alert">
                  {errs.row}
                </span>
              )}
              <div className="flex-1">
                <ObjectRowEditor
                  rowKey={rowKey}
                  fields={fields}
                  value={rowValue}
                  onChange={(next) => onChange({ ...value, [rowKey]: next })}
                  errors={errs?.fields}
                  disabled={disabled}
                />
              </div>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                onClick={() => removeKey(rowKey)}
                disabled={disabled}
                aria-label={`Remove ${rowKey}`}
              >
                <Trash2 className="size-4" aria-hidden />
              </Button>
            </li>
          );
        })}
      </ul>

      <div className="flex flex-wrap items-start gap-2">
        {unusedOptions.length > 0 && (
          <Select onValueChange={(key) => addKey(key)} disabled={disabled} value="">
            <SelectTrigger className="w-56" aria-label={`Add ${spec.label} from list`}>
              <SelectValue placeholder={`Add ${mapKeyHint(spec.keyNoun)}…`} />
            </SelectTrigger>
            <SelectContent>
              {unusedOptions.map((option) => (
                <SelectItem key={option.key} value={option.key}>
                  {option.label || option.key}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
        <div className="flex flex-col gap-1">
          <div className="flex items-center gap-2">
            <Input
              value={draftKey}
              onChange={(e) => {
                setDraftKey(e.target.value);
                if (keyError) setKeyError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  addKey(draftKey);
                }
              }}
              placeholder={`Type a ${mapKeyHint(spec.keyNoun)}`}
              className="w-64"
              disabled={disabled}
              aria-label={`New ${spec.label} key`}
            />
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => addKey(draftKey)}
              disabled={disabled || !draftKey.trim()}
            >
              {draftKey.trim() ? (
                <Check className="size-4" aria-hidden />
              ) : (
                <Plus className="size-4" aria-hidden />
              )}
              Add
            </Button>
          </div>
          {keyError && (
            <span className="text-destructive text-xs" role="alert">
              {keyError}
            </span>
          )}
        </div>
      </div>
    </div>
  );
}

function splitPath(path: string): [string, string | undefined] {
  // Row keys may contain dots (owner/repo does not, but a channel #name
  // could not either; be defensive): the field is the LAST segment only
  // when it names a value field — otherwise the whole path is the row key.
  const idx = path.lastIndexOf(".");
  if (idx === -1) return [path, undefined];
  return [path.slice(0, idx), path.slice(idx + 1)];
}
