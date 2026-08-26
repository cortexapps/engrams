/** The reusable inputs form (ADR 0120): the per-field editors extracted
 * from the Inputs tab so the workstream kickoff dialog renders the SAME
 * form over the same schema/validation stack. `InputsForm` is the bare
 * field list — the surrounding chrome (save buttons, dirty tracking) stays
 * with each caller. */

import { useEffect, useRef, useState } from "react";

import { Field, FieldDescription, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import type { InputFieldError, InputFieldSpec, InputValues } from "@/lib/automation-inputs";

import { ListInputEditor } from "./ListInputEditor";
import { MapInputEditor } from "./MapInputEditor";
import { ScalarValueEditor } from "./ScalarValueEditor";
import { SecretRefPicker } from "./SecretRefPicker";

export function InputsForm({
  schema,
  values,
  errors,
  onChange,
  disabled,
}: {
  schema: InputFieldSpec[];
  values: InputValues;
  errors: InputFieldError[];
  onChange: (key: string, next: unknown) => void;
  disabled: boolean;
}) {
  return (
    <>
      {schema.map((spec) => (
        <InputField
          key={spec.key}
          spec={spec}
          value={values[spec.key]}
          errors={errors.filter((e) => e.key === spec.key)}
          onChange={(next) => onChange(spec.key, next)}
          disabled={disabled}
        />
      ))}
    </>
  );
}

export function InputField({
  spec,
  value,
  errors,
  onChange,
  disabled,
}: {
  spec: InputFieldSpec;
  value: unknown;
  errors: InputFieldError[];
  onChange: (next: unknown) => void;
  disabled: boolean;
}) {
  const topLevel = errors.find((e) => e.path === undefined)?.message;
  return (
    <Field data-invalid={topLevel ? true : undefined} data-testid={`input-${spec.key}`}>
      <FieldLabel>
        {spec.label}
        {spec.required && <span className="text-destructive ml-1">*</span>}
      </FieldLabel>
      {spec.help && <FieldDescription>{spec.help}</FieldDescription>}
      <InputControl
        spec={spec}
        value={value}
        errors={errors}
        onChange={onChange}
        disabled={disabled}
      />
      {topLevel && <FieldError>{topLevel}</FieldError>}
    </Field>
  );
}

function InputControl({
  spec,
  value,
  errors,
  onChange,
  disabled,
}: {
  spec: InputFieldSpec;
  value: unknown;
  errors: InputFieldError[];
  onChange: (next: unknown) => void;
  disabled: boolean;
}) {
  switch (spec.type) {
    case "map":
      return (
        <MapInputEditor
          spec={spec}
          value={
            typeof value === "object" && value !== null ? (value as Record<string, unknown>) : {}
          }
          onChange={onChange}
          errors={errors}
          disabled={disabled}
        />
      );
    case "list":
      return (
        <ListInputEditor
          spec={spec}
          value={Array.isArray(value) ? value : []}
          onChange={onChange}
          errors={errors}
          disabled={disabled}
        />
      );
    case "secret_ref":
      return (
        <SecretRefPicker
          value={typeof value === "string" ? value : ""}
          onChange={onChange}
          disabled={disabled}
          ariaLabel={spec.label}
        />
      );
    case "json":
      return (
        <JsonInput value={value} onChange={onChange} disabled={disabled} ariaLabel={spec.label} />
      );
    case "string":
      if (spec.multiline) {
        return (
          <Textarea
            value={typeof value === "string" ? value : ""}
            onChange={(e) => onChange(e.target.value)}
            rows={6}
            disabled={disabled}
            aria-label={spec.label}
          />
        );
      }
      return (
        <Input
          value={typeof value === "string" ? value : ""}
          onChange={(e) => onChange(e.target.value)}
          disabled={disabled}
          aria-label={spec.label}
        />
      );
    default:
      return (
        <ScalarValueEditor
          field={{
            key: spec.key,
            label: spec.label,
            type: spec.type,
            ...(spec.values ? { values: spec.values } : {}),
          }}
          value={value}
          onChange={onChange}
          disabled={disabled}
        />
      );
  }
}

/** A free JSON object; kept as text while editing, committed when it parses. */
function JsonInput({
  value,
  onChange,
  disabled,
  ariaLabel,
}: {
  value: unknown;
  onChange: (next: unknown) => void;
  disabled: boolean;
  ariaLabel: string;
}) {
  const committed = JSON.stringify(value ?? {}, null, 2);
  const [text, setText] = useState(committed);
  const [bad, setBad] = useState(false);
  // Re-sync to the committed value when the PARENT changes it (Discard, a
  // post-save reload). A keystroke also round-trips through the parent, but
  // comes back SEMANTICALLY equal to what the user typed — so compare parsed
  // values, not text, to leave their formatting and cursor alone; invalid
  // typing never reaches the parent and is never clobbered.
  const lastCommitted = useRef(committed);
  const textRef = useRef(text);
  textRef.current = text;
  useEffect(() => {
    if (lastCommitted.current === committed) return;
    lastCommitted.current = committed;
    try {
      if (JSON.stringify(JSON.parse(textRef.current), null, 2) === committed) return;
    } catch {
      // invalid text is replaced below
    }
    setText(committed);
    setBad(false);
  }, [committed]);
  return (
    <div className="flex flex-col gap-1">
      <Textarea
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          try {
            const parsed: unknown = JSON.parse(e.target.value);
            setBad(false);
            onChange(parsed);
          } catch {
            setBad(true);
          }
        }}
        rows={6}
        className="font-mono text-xs"
        disabled={disabled}
        aria-label={ariaLabel}
      />
      {bad && (
        <span className="text-destructive text-xs" role="alert">
          Not valid JSON
        </span>
      )}
    </div>
  );
}
