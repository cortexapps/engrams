/** The Inputs tab (ADR 0119 phase 3.6 — the built-in editing model).
 *
 * Renders the current version's `inputsSchema` as a form over the
 * automation's `inputs_json`. Editable on EVERY automation, built-ins
 * included: inputs are the per-org knobs a locked graph exposes (the review
 * built-in's repos map, mention, categories, instructions…). Save posts the
 * whole value object through SetInputs. Value validation is CLIENT-side
 * (lib/automation-inputs.ts — see its header): the server currently rejects
 * only undeclared keys, and its errors route back by field key. */

import { ConnectError } from "@connectrpc/connect";
import { Lock, RotateCcw } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Field, FieldDescription, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { useSetInputs } from "@/hooks/useAutomationInputs";
import { useEditorAutomation } from "@/hooks/useAutomationEditor";
import { parseDefinition } from "@/lib/automation-blocks";
import {
  buildInputsPayload,
  inputErrorFromServer,
  inputsEqual,
  parseInputsJson,
  parseInputsSchema,
  resolveInputValues,
  validateInputs,
  type InputFieldError,
  type InputFieldSpec,
  type InputValues,
} from "@/lib/automation-inputs";

import { ListInputEditor } from "./ListInputEditor";
import { MapInputEditor } from "./MapInputEditor";
import { ScalarValueEditor } from "./ScalarValueEditor";
import { SecretRefPicker } from "./SecretRefPicker";

export interface InputsTabProps {
  automationId: string | undefined;
}

export function InputsTab({ automationId }: InputsTabProps) {
  const query = useEditorAutomation(automationId);
  const automation = query.data?.automation;
  const setInputs = useSetInputs();

  const schema = useMemo(
    () => parseInputsSchema(parseDefinition(automation?.version?.definitionJson).inputsSchema),
    [automation],
  );
  const stored = useMemo(
    () => resolveInputValues(schema, parseInputsJson(automation?.inputsJson)),
    [schema, automation],
  );

  const [values, setValues] = useState<InputValues>({});
  const [errors, setErrors] = useState<InputFieldError[]>([]);
  const [loadedKey, setLoadedKey] = useState<string | null>(null);

  // (Re)load when the automation's inputs or version change under us.
  useEffect(() => {
    if (!automation) return;
    const key = `${automation.id}:${automation.currentVersion}:${automation.inputsJson}`;
    if (key === loadedKey) return;
    setLoadedKey(key);
    setValues(stored);
    setErrors([]);
  }, [automation, stored, loadedKey]);

  if (!automationId) {
    return (
      <p className="text-muted-foreground rounded-lg border border-dashed p-6 text-sm">
        Save the automation first; inputs are set on a saved automation.
      </p>
    );
  }
  if (query.isLoading || !automation) {
    return <Skeleton className="h-40 w-full" data-testid="inputs-loading" />;
  }
  if (schema.length === 0) {
    return (
      <p className="text-muted-foreground rounded-lg border border-dashed p-6 text-sm">
        This automation declares no inputs. Inputs are the per-org settings a locked graph exposes —
        add them to the definition's input schema.
      </p>
    );
  }

  const dirty = !inputsEqual(values, stored);
  const builtin = automation.kind === "builtin";

  const update = (key: string, next: unknown) => {
    setValues((prev) => ({ ...prev, [key]: next }));
    setErrors((prev) => prev.filter((e) => e.key !== key));
  };

  const save = async () => {
    const clientErrors = validateInputs(schema, values);
    if (clientErrors.length > 0) {
      setErrors(clientErrors);
      toast.error("Fix the highlighted inputs");
      return;
    }
    try {
      await setInputs.mutateAsync({
        automationId: automation.id,
        inputsJson: buildInputsPayload(schema, values),
      });
      setErrors([]);
      toast.success("Inputs saved");
    } catch (error) {
      const message =
        error instanceof ConnectError
          ? error.rawMessage
          : error instanceof Error
            ? error.message
            : String(error);
      const routed = message
        .split("; ")
        .map((part) => {
          const m = /^([A-Za-z0-9_.\-/#]+): (.*)$/.exec(part);
          return m ? inputErrorFromServer(m[1]!, m[2]!, schema) : null;
        })
        .filter((e): e is InputFieldError => e !== null);
      setErrors(routed);
      toast.error(routed.length > 0 ? "Fix the highlighted inputs" : message);
    }
  };

  return (
    <form
      className="flex max-w-3xl flex-col gap-6"
      data-testid="inputs-tab"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      {builtin && (
        <p className="text-muted-foreground flex items-center gap-2 text-sm">
          <Lock className="size-3.5" aria-hidden />
          This is a built-in: its graph is locked, and these inputs are how you configure it.
        </p>
      )}

      {schema.map((spec) => (
        <InputField
          key={spec.key}
          spec={spec}
          value={values[spec.key]}
          errors={errors.filter((e) => e.key === spec.key)}
          onChange={(next) => update(spec.key, next)}
          disabled={setInputs.isPending}
        />
      ))}

      <div className="flex items-center gap-2">
        <Button type="submit" disabled={!dirty || setInputs.isPending}>
          {setInputs.isPending ? "Saving…" : "Save inputs"}
        </Button>
        <Button
          type="button"
          variant="ghost"
          disabled={!dirty || setInputs.isPending}
          onClick={() => {
            setValues(stored);
            setErrors([]);
          }}
        >
          <RotateCcw className="size-4" aria-hidden />
          Discard
        </Button>
        {dirty && <span className="text-muted-foreground text-xs">Unsaved changes</span>}
      </div>
    </form>
  );
}

function InputField({
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
