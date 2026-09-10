/** The Inputs tab (ADR 0119 phase 3.6 — the built-in editing model).
 *
 * Renders the current version's `inputsSchema` as a form over the
 * automation's `inputs_json`. Editable on EVERY automation, built-ins
 * included: inputs are the per-org knobs a locked graph exposes (the review
 * built-in's repos map, mention, categories, instructions…). Save posts the
 * whole value object through SetInputs; the server re-validates values with
 * the same rules and its errors route back by field key (and by row/field
 * path for maps and lists). */

import { ConnectError } from "@connectrpc/connect";
import { Lock, RotateCcw } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { toast } from "sonner";

import { EmptyState } from "@/components/empty-state";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
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
  type InputValues,
} from "@/lib/automation-inputs";

import { InputField } from "./InputsForm";

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
      <EmptyState>Save the automation first; inputs are set on a saved automation.</EmptyState>
    );
  }
  if (query.isLoading || !automation) {
    return <Skeleton className="h-40 w-full" data-testid="inputs-loading" />;
  }
  if (schema.length === 0) {
    return (
      <EmptyState>
        This automation declares no inputs. Inputs are the per-org settings a locked graph exposes —
        add them to the definition's input schema.
      </EmptyState>
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
