/** Generic typed-field renderer for the block inspector (ADR 0119 phase 3.3).
 *
 * One component per FieldSpec type, all sharing the same props so the
 * inspector composes them from a registry entry's `fields`. `pinned` renders
 * the value read-only with the built-in hint (the editing model: structure
 * locked, properties editable only where `tunable`). */

import { Lock } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";

import { Field, FieldDescription, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { useProfiles } from "@/hooks/useProfiles";
import type { FieldSpec } from "@/lib/automation-blocks";

import { VariablePicker } from "./VariablePicker";

export interface GenericFieldProps {
  spec: FieldSpec;
  value: unknown;
  onChange: (next: unknown) => void;
  /** Read-only with the "Set by the built-in" hint. */
  pinned: boolean;
  error?: string;
  /** Block ids above this one whose session_id output can be referenced. */
  sessionSources: readonly string[];
  /** Static variable paths the picker offers (3.4 adds live values). */
  variablePaths: readonly string[];
  variableValues?: Readonly<Record<string, string>>;
}

function Shell({
  spec,
  pinned,
  error,
  children,
}: {
  spec: FieldSpec;
  pinned: boolean;
  error?: string;
  children: ReactNode;
}) {
  return (
    <Field data-invalid={error ? true : undefined} data-testid={`field-${spec.key}`}>
      <FieldLabel className="flex items-center gap-1.5">
        {spec.label}
        {pinned && (
          <span className="text-muted-foreground inline-flex items-center gap-1 text-xs font-normal">
            <Lock className="size-3" aria-hidden />
            Set by the built-in
          </span>
        )}
      </FieldLabel>
      {children}
      {spec.help && !error && <FieldDescription>{spec.help}</FieldDescription>}
      {error && <FieldError>{error}</FieldError>}
    </Field>
  );
}

function asString(value: unknown): string {
  return typeof value === "string"
    ? value
    : value === undefined || value === null
      ? ""
      : String(value);
}

export function GenericField(props: GenericFieldProps) {
  const { spec, value, onChange, pinned, error, sessionSources, variablePaths, variableValues } =
    props;
  const disabled = pinned;

  switch (spec.type) {
    case "template":
      return (
        <Shell spec={spec} pinned={pinned} error={error}>
          {spec.multiline ? (
            <Textarea
              value={asString(value)}
              onChange={(e) => onChange(e.target.value)}
              disabled={disabled}
              rows={5}
              className="font-mono text-sm"
            />
          ) : (
            <Input
              value={asString(value)}
              onChange={(e) => onChange(e.target.value)}
              disabled={disabled}
            />
          )}
          {!disabled && (
            <VariablePicker
              paths={variablePaths}
              values={variableValues}
              onInsert={(path) => onChange(`${asString(value)}\${{ ${path} }}`)}
            />
          )}
        </Shell>
      );
    case "string":
    case "secret_ref":
      return (
        <Shell spec={spec} pinned={pinned} error={error}>
          <Input
            value={asString(value)}
            onChange={(e) => onChange(e.target.value)}
            disabled={disabled}
          />
        </Shell>
      );
    case "number":
    case "duration":
      return (
        <Shell spec={spec} pinned={pinned} error={error}>
          <Input
            type="number"
            inputMode="numeric"
            value={typeof value === "number" ? String(value) : ""}
            min={spec.type === "number" ? spec.min : 1}
            max={spec.type === "number" ? spec.max : undefined}
            placeholder={spec.type === "duration" ? "seconds" : undefined}
            onChange={(e) => onChange(e.target.value === "" ? undefined : Number(e.target.value))}
            disabled={disabled}
          />
        </Shell>
      );
    case "boolean":
      return (
        <Shell spec={spec} pinned={pinned} error={error}>
          <Switch
            checked={value === true}
            onCheckedChange={(checked) => onChange(checked)}
            disabled={disabled}
          />
        </Shell>
      );
    case "select":
      return (
        <Shell spec={spec} pinned={pinned} error={error}>
          <Select
            value={asString(value)}
            onValueChange={(next) => onChange(next)}
            disabled={disabled}
          >
            <SelectTrigger>
              <SelectValue placeholder="Choose…" />
            </SelectTrigger>
            <SelectContent>
              {spec.options.map((option) => (
                <SelectItem key={option} value={option}>
                  {option.replaceAll("_", " ")}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Shell>
      );
    case "session_ref": {
      const ref = (typeof value === "object" && value !== null ? value : {}) as {
        blockId?: string;
        template?: string;
      };
      const mode = ref.template !== undefined ? "template" : "block";
      return (
        <Shell spec={spec} pinned={pinned} error={error}>
          <div className="flex gap-2">
            <Select
              value={mode === "block" ? (ref.blockId ?? "") : "__template__"}
              onValueChange={(next) =>
                onChange(
                  next === "__template__" ? { template: ref.template ?? "" } : { blockId: next },
                )
              }
              disabled={disabled}
            >
              <SelectTrigger className="flex-1">
                <SelectValue placeholder="Session from block…" />
              </SelectTrigger>
              <SelectContent>
                {sessionSources.map((id) => (
                  <SelectItem key={id} value={id}>
                    session from “{id}”
                  </SelectItem>
                ))}
                <SelectItem value="__template__">template…</SelectItem>
              </SelectContent>
            </Select>
            {mode === "template" && (
              <Input
                className="flex-1 font-mono text-sm"
                placeholder="${{ steps.x.session_id }}"
                value={ref.template ?? ""}
                onChange={(e) => onChange({ template: e.target.value })}
                disabled={disabled}
              />
            )}
          </div>
        </Shell>
      );
    }
    case "profile":
      return <ProfileField {...props} />;
    case "json":
      return <JsonField {...props} />;
  }
}

function ProfileField({ spec, value, onChange, pinned, error }: GenericFieldProps) {
  const profiles = useProfiles();
  return (
    <Shell spec={spec} pinned={pinned} error={error}>
      <Select value={asString(value)} onValueChange={(next) => onChange(next)} disabled={pinned}>
        <SelectTrigger data-testid="profile-select">
          <SelectValue placeholder="Choose a profile…" />
        </SelectTrigger>
        <SelectContent>
          {(profiles.data?.profiles ?? []).map((profile) => (
            <SelectItem key={profile.id} value={profile.id}>
              {profile.name}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </Shell>
  );
}

function JsonField({ spec, value, onChange, pinned, error }: GenericFieldProps) {
  // The textarea owns its text: a fully controlled field that only commits on
  // a valid parse reverts every intermediate keystroke (almost all partial
  // JSON is invalid), which made the field un-typeable. Keystrokes always
  // land locally; the config updates only on a valid parse; an external
  // change to `value` (discard, reload) re-syncs the text — compared
  // SEMANTICALLY so a valid mid-edit keeps its formatting and cursor.
  const committed = value === undefined ? "" : JSON.stringify(value, null, 2);
  const [text, setText] = useState(committed);
  const [invalid, setInvalid] = useState(false);
  const lastCommitted = useRef(committed);
  const textRef = useRef(text);
  textRef.current = text;
  useEffect(() => {
    if (lastCommitted.current === committed) return;
    lastCommitted.current = committed;
    if (textRef.current.trim() === "" && committed === "") return;
    try {
      if (JSON.stringify(JSON.parse(textRef.current), null, 2) === committed) return;
    } catch {
      // invalid text is replaced by the committed value
    }
    setText(committed);
    setInvalid(false);
  }, [committed]);
  return (
    <Shell spec={spec} pinned={pinned} error={error}>
      <Textarea
        value={text}
        rows={4}
        className="font-mono text-sm"
        disabled={pinned}
        aria-invalid={invalid || undefined}
        onChange={(e) => {
          const raw = e.target.value;
          setText(raw);
          if (raw.trim() === "") {
            setInvalid(false);
            onChange(undefined);
            return;
          }
          try {
            const parsed: unknown = JSON.parse(raw);
            setInvalid(false);
            onChange(parsed);
          } catch {
            setInvalid(true);
          }
        }}
      />
      {invalid && <FieldError>Not valid JSON</FieldError>}
    </Shell>
  );
}
