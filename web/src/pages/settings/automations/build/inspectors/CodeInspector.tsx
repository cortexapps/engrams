/** The code block's inspector (ADR 0119 phase 3.5): CodeMirror editor, the
 * value|boolean mode toggle, and a Run button that evaluates the source in
 * the real sandbox (EvalCode) against the selected sample's scope.
 *
 * Scope resolution: 3.4's test panel hands down `evalScope` when a sample is
 * selected; until then the inspector falls back to the automation's inputs
 * plus its latest ledgered sample, so Run always has something real to work
 * with. Errors land on the offending line as a CodeMirror diagnostic. */

import { Lock, Play } from "lucide-react";
import { useParams } from "@tanstack/react-router";
import { useMemo, useState } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Field, FieldDescription, FieldError, FieldLabel } from "@/components/ui/field";
import {
  evalInputJson,
  evalOutcome,
  useEvalCode,
  type EvalOutcome,
  type EvalScope,
} from "@/hooks/useAutomationCode";
import { useEditorAutomation } from "@/hooks/useAutomationEditor";
import { useEventSamples } from "@/hooks/useAutomations";
import { isTunable, setPath, type BlockDef, type BlockErrorRef } from "@/lib/automation-blocks";

import { CodeEditorLazy } from "../CodeEditorLazy";
import { GenericField } from "../fields/GenericField";

export interface CodeInspectorProps {
  block: BlockDef;
  onChange: (next: BlockDef) => void;
  builtin: boolean;
  errors: readonly BlockErrorRef[];
  sessionSources: readonly string[];
  variablePaths: readonly string[];
  /** 3.4: the selected sample's scope. Absent → latest-sample fallback. */
  evalScope?: EvalScope;
}

function errorFor(errors: readonly BlockErrorRef[], field: string): string | undefined {
  return errors.find((e) => e.field === field || e.field.startsWith(`${field}.`))?.message;
}

function parseJsonObject(text: string | undefined): Record<string, unknown> {
  if (!text) return {};
  try {
    const parsed: unknown = JSON.parse(text);
    return typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : {};
  } catch {
    return {};
  }
}

/** Latest-sample fallback scope (used when 3.4 has not supplied one). */
function useFallbackScope(enabled: boolean): EvalScope | undefined {
  const params = useParams({ strict: false }) as { id?: string };
  const automation = useEditorAutomation(enabled ? params.id : undefined);
  const samples = useEventSamples(enabled ? params.id : undefined, 1);
  return useMemo(() => {
    if (!enabled) return undefined;
    const latest = samples.data?.samples[0];
    const raw = parseJsonObject(latest?.payloadJson);
    return {
      inputs: parseJsonObject(automation.data?.automation?.inputsJson),
      trigger: latest
        ? { kind: "integration", event: latest.eventKey, received_at: latest.receivedAt }
        : { kind: "manual" },
      event: { raw },
      steps: {},
    };
  }, [enabled, automation.data, samples.data]);
}

function prettyJson(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    return String(value);
  }
}

export function CodeInspector(props: CodeInspectorProps) {
  const { block, onChange, builtin, errors } = props;
  const sourcePinned = builtin && !isTunable(block, "source");
  const modePinned = builtin && !isTunable(block, "mode");
  const source = typeof block.config["source"] === "string" ? block.config["source"] : "";
  const mode = block.config["mode"] === "boolean" ? "boolean" : "value";

  const fallbackScope = useFallbackScope(props.evalScope === undefined);
  const scope = props.evalScope ?? fallbackScope;
  const evaluate = useEvalCode();
  const [outcome, setOutcome] = useState<EvalOutcome | null>(null);

  const run = () => {
    setOutcome(null);
    evaluate.mutate(
      { source, mode, inputJson: evalInputJson(scope ?? {}) },
      {
        onSuccess: (response) => setOutcome(evalOutcome(response)),
        onError: (error) =>
          setOutcome({
            ok: false,
            error: { name: "RequestError", message: error.message },
            logs: [],
            durationMs: 0,
          }),
      },
    );
  };

  const runError = outcome && !outcome.ok ? outcome.error : undefined;

  return (
    <div className="flex flex-col gap-3">
      <GenericField
        spec={{
          type: "select",
          key: "mode",
          label: "Mode",
          options: ["value", "boolean"],
          help: "boolean: returning false ends the run as filtered.",
        }}
        value={mode}
        onChange={(value) => onChange({ ...block, config: setPath(block.config, "mode", value) })}
        pinned={modePinned}
        error={errorFor(errors, "mode")}
        sessionSources={props.sessionSources}
        variablePaths={props.variablePaths}
      />

      <Field
        data-invalid={errorFor(errors, "source") ? true : undefined}
        data-testid="field-source"
      >
        <div className="flex items-center justify-between gap-2">
          <FieldLabel className="flex items-center gap-1.5">
            Source
            {sourcePinned && (
              <span className="text-muted-foreground inline-flex items-center gap-1 text-xs font-normal">
                <Lock className="size-3" aria-hidden /> Set by the built-in
              </span>
            )}
          </FieldLabel>
          <Button
            type="button"
            size="sm"
            variant="outline"
            onClick={run}
            disabled={evaluate.isPending || source.trim() === ""}
            data-testid="code-run"
          >
            <Play className="size-3.5" aria-hidden />
            {evaluate.isPending ? "Running…" : "Run"}
          </Button>
        </div>
        <CodeEditorLazy
          value={source}
          onChange={(next) => onChange({ ...block, config: setPath(block.config, "source", next) })}
          readOnly={sourcePinned}
          {...(runError?.line !== undefined
            ? { errorLine: runError.line, errorMessage: `${runError.name}: ${runError.message}` }
            : {})}
          aria-label="Code source"
        />
        {errorFor(errors, "source") ? (
          <FieldError>{errorFor(errors, "source")}</FieldError>
        ) : (
          <FieldDescription>
            <code>
              export default ({"{"} event, inputs, steps, trigger {"}"}) =&gt; value
            </code>{" "}
            — no network, no timers; 250 ms CPU, 32 MiB.
          </FieldDescription>
        )}
      </Field>

      {outcome && (
        <div
          className="bg-muted flex flex-col gap-2 rounded-lg border p-3 text-xs"
          data-testid="code-result"
          data-ok={outcome.ok}
        >
          <div className="flex items-center gap-2">
            <Badge variant={outcome.ok ? "secondary" : "destructive"}>
              {outcome.ok ? "value" : outcome.error.name}
            </Badge>
            <span className="text-muted-foreground">
              {outcome.durationMs} ms
              {!outcome.ok && outcome.error.line !== undefined && ` · line ${outcome.error.line}`}
            </span>
          </div>
          {outcome.ok ? (
            <pre className="max-h-64 overflow-auto font-mono whitespace-pre-wrap">
              {prettyJson(outcome.value)}
            </pre>
          ) : (
            <p className="text-instrument-critical-ink font-mono" data-testid="code-error">
              {outcome.error.message}
            </p>
          )}
          {outcome.logs.length > 0 && (
            <details>
              <summary className="text-muted-foreground cursor-pointer">
                console ({outcome.logs.length})
              </summary>
              <pre className="mt-1 max-h-40 overflow-auto font-mono whitespace-pre-wrap">
                {outcome.logs.join("\n")}
              </pre>
            </details>
          )}
        </div>
      )}
    </div>
  );
}
