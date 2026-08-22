/** The right-hand inspector for one selected block (ADR 0119 phase 3.3).
 *
 * Dispatches on the client registry: generic typed-field forms for most
 * kinds, custom inspectors for create_session / branch / loop / filter /
 * code / integration_action, and a read-only view for system blocks.
 *
 * Editing model: on a built-in, a field is editable iff its top-level config
 * key is in the block's `tunable` list; everything else is pinned. */

import { Lock } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { Badge } from "@/components/ui/badge";
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
import { useActionCatalog } from "@/hooks/useAutomationEditor";
import { useHarnessCatalog } from "@/hooks/useHarnessCatalog";
import { useProfiles } from "@/hooks/useProfiles";
import {
  blockKind,
  getPath,
  isTunable,
  setPath,
  type BlockDef,
  type BlockErrorRef,
  type FieldSpec,
} from "@/lib/automation-blocks";
import {
  SessionHarnessControls,
  type HarnessOverride,
} from "@/pages/sessions/SessionHarnessControls";

import { asFilterGroup, ConditionEditor } from "./fields/ConditionEditor";
import { CodeInspector } from "./inspectors/CodeInspector";
import { GenericField } from "./fields/GenericField";
import { VariablePicker } from "./fields/VariablePicker";

export interface BlockInspectorProps {
  block: BlockDef;
  onChange: (next: BlockDef) => void;
  /** Built-in: structure locked, only tunable fields editable. */
  builtin: boolean;
  errors: readonly BlockErrorRef[];
  /** Block ids above this one that expose a session_id output. */
  sessionSources: readonly string[];
  /** Static variable paths for pickers and condition rows. */
  variablePaths: readonly string[];
  /** 3.4: live preview values keyed by path. */
  variableValues?: Readonly<Record<string, string>>;
}

/** Whether a field (by dotted path) may be edited under the current mode. */
function editable(block: BlockDef, builtin: boolean, fieldPath: string): boolean {
  return builtin ? isTunable(block, fieldPath) : true;
}

function errorFor(errors: readonly BlockErrorRef[], field: string): string | undefined {
  return errors.find((e) => e.field === field || e.field.startsWith(`${field}.`))?.message;
}

export function BlockInspector(props: BlockInspectorProps) {
  const { block, builtin, errors } = props;
  const spec = blockKind(block.type);
  const Icon = spec.icon;
  const formError = errors.find((e) => e.field === "" || e.field === "config")?.message;

  return (
    <div className="flex flex-col gap-4" data-testid="block-inspector">
      <header className="flex items-start gap-3">
        <Icon className="text-muted-foreground mt-0.5 size-5 shrink-0" aria-hidden />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <h3 className="truncate font-medium">{spec.label}</h3>
            <code className="text-muted-foreground text-xs">{block.id}</code>
            {spec.system && <Badge variant="secondary">built-in logic</Badge>}
          </div>
          <p className="text-muted-foreground text-sm">{spec.description}</p>
          {builtin && !spec.system && (
            <p className="text-muted-foreground mt-1 flex items-center gap-1 text-xs">
              <Lock className="size-3" aria-hidden />
              {(block.tunable?.length ?? 0) > 0
                ? `Editable here: ${block.tunable!.join(", ")}`
                : "Every property of this block is set by the built-in"}
            </p>
          )}
        </div>
      </header>

      {formError && <FieldError>{formError}</FieldError>}

      {spec.inspector === "system" ? (
        <SystemView block={block} />
      ) : spec.inspector === "create_session" ? (
        <CreateSessionInspector {...props} />
      ) : spec.inspector === "filter" ||
        spec.inspector === "branch" ||
        spec.inspector === "loop" ? (
        <ConditionInspector {...props} kind={spec.inspector} />
      ) : spec.inspector === "code" ? (
        <CodeInspector {...props} />
      ) : spec.inspector === "integration_action" ? (
        <IntegrationActionInspector {...props} />
      ) : (
        <GenericForm {...props} fields={spec.fields ?? []} />
      )}

      {!spec.system && (
        <RetryEditor
          block={block}
          onChange={props.onChange}
          disabled={builtin && !isTunable(block, "retry")}
        />
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------

function GenericForm(props: BlockInspectorProps & { fields: FieldSpec[] }) {
  const {
    block,
    onChange,
    builtin,
    errors,
    fields,
    sessionSources,
    variablePaths,
    variableValues,
  } = props;
  return (
    <div className="flex flex-col gap-3">
      {fields.map((field) => (
        <GenericField
          key={field.key}
          spec={field}
          value={getPath(block.config, field.key)}
          onChange={(value) =>
            onChange({ ...block, config: setPath(block.config, field.key, value) })
          }
          pinned={!editable(block, builtin, field.key)}
          error={errorFor(errors, field.key)}
          sessionSources={sessionSources}
          variablePaths={variablePaths}
          variableValues={variableValues}
        />
      ))}
    </div>
  );
}

function SystemView({ block }: { block: BlockDef }) {
  return (
    <div className="rounded-md border p-3">
      <p className="text-muted-foreground mb-2 text-xs">Configuration (read-only)</p>
      <pre className="overflow-x-auto text-xs">{JSON.stringify(block.config, null, 2)}</pre>
    </div>
  );
}

// ---------------------------------------------------------------------------
// create_session — profile + harness override controls + prompt
// ---------------------------------------------------------------------------

const CREATE_SESSION_FIELDS: FieldSpec[] = [
  { type: "template", key: "promptTemplate", label: "Initial prompt", multiline: true },
  {
    type: "template",
    key: "titleTemplate",
    label: "Task title",
    help: "Optional; defaults to the prompt.",
  },
  {
    type: "boolean",
    key: "includeEventContext",
    label: "Append event context",
    help: "Adds the redacted trigger payload to the prompt, marked as untrusted data.",
  },
  {
    type: "string",
    key: "role",
    label: "Role",
    help: "Label for this session within the run (default: primary).",
  },
  {
    type: "boolean",
    key: "keepOnFinish",
    label: "Keep session when the run ends",
    help: "Sessions are kept by default; this overrides the automation's setting for this block.",
  },
];

function CreateSessionInspector(props: BlockInspectorProps) {
  const { block, onChange, builtin, errors } = props;
  const profiles = useProfiles();
  const harnesses = useHarnessCatalog();
  const profile = profiles.data?.profiles.find((p) => p.id === block.config["profileId"]);

  const override: HarnessOverride = useMemo(
    () => ({
      harness: (block.config["harness"] as string | undefined) ?? null,
      model: (block.config["model"] as string | undefined) ?? null,
      modelRouter: (block.config["modelRouter"] as string | undefined) ?? null,
      effort: (block.config["effort"] as string | undefined) ?? null,
      mode: (block.config["harnessMode"] as string | undefined) ?? null,
    }),
    [block.config],
  );

  const setOverride = (next: HarnessOverride) => {
    let config = block.config;
    const assign = (key: string, value: string | null | undefined) => {
      config = setPath(config, key, value ?? undefined);
    };
    assign("harness", next.harness);
    assign("model", next.model);
    assign("modelRouter", next.modelRouter === null ? undefined : next.modelRouter);
    assign("effort", next.effort);
    assign("harnessMode", next.mode);
    onChange({ ...block, config });
  };

  const profilePinned = !editable(block, builtin, "profileId");
  const harnessPinned = !editable(block, builtin, "harness");

  return (
    <div className="flex flex-col gap-3">
      <GenericField
        spec={{ type: "profile", key: "profileId", label: "Profile" }}
        value={block.config["profileId"]}
        onChange={(value) =>
          onChange({ ...block, config: setPath(block.config, "profileId", value) })
        }
        pinned={profilePinned}
        error={errorFor(errors, "profileId")}
        sessionSources={props.sessionSources}
        variablePaths={props.variablePaths}
      />
      <Field>
        <FieldLabel className="flex items-center gap-1.5">
          Harness override
          {harnessPinned && (
            <span className="text-muted-foreground inline-flex items-center gap-1 text-xs font-normal">
              <Lock className="size-3" aria-hidden /> Set by the built-in
            </span>
          )}
        </FieldLabel>
        <SessionHarnessControls
          harnesses={harnesses.data}
          profileHarness={profile?.harness || undefined}
          profileModel={profile?.model || undefined}
          value={override}
          onChange={setOverride}
          disabled={harnessPinned}
          audience="programmatic"
        />
        <FieldDescription>
          {builtin && !harnessPinned
            ? "Clearing a field reverts it to the built-in's shipped default (an override only tunes a value; it cannot unset one)."
            : "Leave unset to inherit the profile's harness, model, and effort."}
        </FieldDescription>
      </Field>
      <GenericForm {...props} fields={CREATE_SESSION_FIELDS} />
    </div>
  );
}

// ---------------------------------------------------------------------------
// filter / branch / loop — structured conditions
// ---------------------------------------------------------------------------

function ConditionInspector(props: BlockInspectorProps & { kind: "filter" | "branch" | "loop" }) {
  const { block, onChange, builtin, errors, kind, variablePaths } = props;
  const condKey = kind === "loop" ? "until" : "conditions";
  const pinned = !editable(block, builtin, condKey);
  const error = errorFor(errors, condKey);

  return (
    <div className="flex flex-col gap-3">
      {kind === "loop" && (
        <GenericField
          spec={{ type: "number", key: "maxIterations", label: "Max iterations", min: 1, max: 100 }}
          value={block.config["maxIterations"]}
          onChange={(value) =>
            onChange({ ...block, config: setPath(block.config, "maxIterations", value) })
          }
          pinned={!editable(block, builtin, "maxIterations")}
          error={errorFor(errors, "maxIterations")}
          sessionSources={props.sessionSources}
          variablePaths={variablePaths}
        />
      )}
      <Field data-invalid={error ? true : undefined} data-testid={`field-${condKey}`}>
        <FieldLabel className="flex items-center gap-1.5">
          {kind === "filter"
            ? "Continue when"
            : kind === "branch"
              ? "Take the then-branch when"
              : "Stop looping when"}
          {pinned && (
            <span className="text-muted-foreground inline-flex items-center gap-1 text-xs font-normal">
              <Lock className="size-3" aria-hidden /> Set by the built-in
            </span>
          )}
        </FieldLabel>
        <ConditionEditor
          value={asFilterGroup(block.config[condKey])}
          onChange={(next) => onChange({ ...block, config: setPath(block.config, condKey, next) })}
          disabled={pinned}
          paths={variablePaths}
        />
        {error ? (
          <FieldError>{error}</FieldError>
        ) : (
          <FieldDescription>
            {kind === "filter"
              ? "No match ends the run as “filtered”. For anything rows cannot say, use a Code block in boolean mode."
              : kind === "loop"
                ? "Optional. Without it the loop runs to the iteration cap."
                : "Paths resolve against inputs, trigger, event, and steps."}
          </FieldDescription>
        )}
      </Field>
    </div>
  );
}

// ---------------------------------------------------------------------------
// integration_action — provider + action from the catalog, params from its
// input schema (flat object root; nested schemas fall back to JSON)
// ---------------------------------------------------------------------------

interface FieldSchema {
  type?: string;
  properties?: Record<string, FieldSchema>;
  required?: string[];
  enum?: string[];
}

const PROVIDERS = ["github", "slack", "linear"] as const;

function IntegrationActionInspector(props: BlockInspectorProps) {
  const { block, onChange, builtin, errors, variablePaths } = props;
  const provider = typeof block.config["provider"] === "string" ? block.config["provider"] : "";
  const actionId = typeof block.config["actionId"] === "string" ? block.config["actionId"] : "";
  const catalog = useActionCatalog(provider || undefined);
  const action = catalog.data?.actions.find((a) => a.id === actionId);
  const schema = useMemo<FieldSchema | undefined>(() => {
    if (!action?.inputSchemaJson) return undefined;
    try {
      return JSON.parse(action.inputSchemaJson) as FieldSchema;
    } catch {
      return undefined;
    }
  }, [action]);
  const params = (block.config["params"] ?? {}) as Record<string, unknown>;
  const providerPinned = !editable(block, builtin, "provider");
  const actionPinned = !editable(block, builtin, "actionId");
  const paramsPinned = !editable(block, builtin, "params");

  const setParam = (key: string, value: unknown) =>
    onChange({ ...block, config: { ...block.config, params: { ...params, [key]: value } } });
  // Clearing a param removes the key so the action's `required` check (not an
  // empty string / NaN) decides whether it is missing.
  const clearParam = (key: string) => {
    const { [key]: _dropped, ...rest } = params;
    onChange({ ...block, config: { ...block.config, params: rest } });
  };

  return (
    <div className="flex flex-col gap-3">
      <Field data-testid="field-provider">
        <FieldLabel>Integration</FieldLabel>
        <Select
          value={provider}
          onValueChange={(next) =>
            onChange({
              ...block,
              config: { ...block.config, provider: next, actionId: "", params: {} },
            })
          }
          disabled={providerPinned}
        >
          <SelectTrigger>
            <SelectValue placeholder="Choose…" />
          </SelectTrigger>
          <SelectContent>
            {PROVIDERS.map((p) => (
              <SelectItem key={p} value={p}>
                {p}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </Field>
      <Field
        data-invalid={errorFor(errors, "actionId") ? true : undefined}
        data-testid="field-actionId"
      >
        <FieldLabel>Action</FieldLabel>
        <Select
          value={actionId}
          onValueChange={(next) =>
            onChange({ ...block, config: { ...block.config, actionId: next, params: {} } })
          }
          disabled={actionPinned || !provider}
        >
          <SelectTrigger>
            <SelectValue
              placeholder={provider ? "Choose an action…" : "Choose an integration first"}
            />
          </SelectTrigger>
          <SelectContent>
            {(catalog.data?.actions ?? []).map((a) => (
              <SelectItem key={a.id} value={a.id}>
                {a.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        {action?.description && !errorFor(errors, "actionId") && (
          <FieldDescription>{action.description}</FieldDescription>
        )}
        {errorFor(errors, "actionId") && <FieldError>{errorFor(errors, "actionId")}</FieldError>}
      </Field>

      {schema?.properties ? (
        Object.entries(schema.properties).map(([key, prop]) => {
          const required = schema.required?.includes(key);
          const label = `${key}${required ? "" : " (optional)"}`;
          const error = errorFor(errors, `params.${key}`);
          const value = params[key];
          if (prop.type === "boolean") {
            return (
              <Field key={key} data-testid={`field-params.${key}`}>
                <FieldLabel>{label}</FieldLabel>
                <Switch
                  checked={value === true}
                  onCheckedChange={(v) => setParam(key, v)}
                  disabled={paramsPinned}
                />
              </Field>
            );
          }
          if (prop.enum) {
            return (
              <Field key={key} data-testid={`field-params.${key}`}>
                <FieldLabel>{label}</FieldLabel>
                <Select
                  value={typeof value === "string" ? value : ""}
                  onValueChange={(v) => setParam(key, v)}
                  disabled={paramsPinned}
                >
                  <SelectTrigger>
                    <SelectValue placeholder="Choose…" />
                  </SelectTrigger>
                  <SelectContent>
                    {prop.enum.map((o) => (
                      <SelectItem key={o} value={o}>
                        {o}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </Field>
            );
          }
          if (prop.type === "number" || prop.type === "integer") {
            // The action validates params against its schema at RUN time
            // (validateFieldValue rejects a numeric string), so the inspector
            // must write a real number. A Liquid template is also accepted —
            // it renders to a number before validation.
            return (
              <NumericParamField
                key={key}
                label={label}
                integer={prop.type === "integer"}
                value={value}
                pinned={paramsPinned}
                error={error}
                variablePaths={variablePaths}
                testId={`field-params.${key}`}
                onChange={(next) => (next === undefined ? clearParam(key) : setParam(key, next))}
              />
            );
          }
          if (prop.type === "string") {
            const text = typeof value === "string" ? value : "";
            return (
              <Field
                key={key}
                data-invalid={error ? true : undefined}
                data-testid={`field-params.${key}`}
              >
                <FieldLabel>{label}</FieldLabel>
                {key === "body" || key === "summary" || key === "text" ? (
                  <Textarea
                    value={text}
                    rows={4}
                    className="font-mono text-sm"
                    onChange={(e) => setParam(key, e.target.value)}
                    disabled={paramsPinned}
                  />
                ) : (
                  <Input
                    value={text}
                    onChange={(e) => setParam(key, e.target.value)}
                    disabled={paramsPinned}
                  />
                )}
                {!paramsPinned && (
                  <VariablePicker
                    paths={variablePaths}
                    onInsert={(p) => setParam(key, `${text}\${{ ${p} }}`)}
                  />
                )}
                {error && <FieldError>{error}</FieldError>}
              </Field>
            );
          }
          // Arrays / objects: JSON.
          return (
            <GenericField
              key={key}
              spec={{ type: "json", key: `params.${key}`, label }}
              value={value}
              onChange={(v) => setParam(key, v)}
              pinned={paramsPinned}
              error={error}
              sessionSources={props.sessionSources}
              variablePaths={variablePaths}
            />
          );
        })
      ) : actionId ? (
        <GenericField
          spec={{ type: "json", key: "params", label: "Parameters" }}
          value={block.config["params"]}
          onChange={(v) => onChange({ ...block, config: setPath(block.config, "params", v ?? {}) })}
          pinned={paramsPinned}
          error={errorFor(errors, "params")}
          sessionSources={props.sessionSources}
          variablePaths={variablePaths}
        />
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Numeric action param — writes a number (or a template), never a string
// ---------------------------------------------------------------------------

const TEMPLATE_RE = /\$\{\{/;

function NumericParamField({
  label,
  integer,
  value,
  pinned,
  error,
  variablePaths,
  testId,
  onChange,
}: {
  label: string;
  integer: boolean;
  value: unknown;
  pinned: boolean;
  error: string | undefined;
  variablePaths: readonly string[];
  testId: string;
  onChange: (next: number | string | undefined) => void;
}) {
  // Local text so a half-typed value ("-", "1e") never reaches the config as
  // NaN; the committed value is a number, a template string, or absent.
  const committed =
    typeof value === "number" ? String(value) : typeof value === "string" ? value : "";
  const [text, setText] = useState(committed);
  const lastCommitted = useRef(committed);
  useEffect(() => {
    if (lastCommitted.current === committed) return;
    lastCommitted.current = committed;
    setText(committed);
  }, [committed]);
  const invalid = text !== "" && !TEMPLATE_RE.test(text) && !isFiniteNumeric(text, integer);
  const apply = (raw: string) => {
    setText(raw);
    if (raw === "") {
      onChange(undefined);
      return;
    }
    if (TEMPLATE_RE.test(raw)) {
      onChange(raw);
      return;
    }
    if (isFiniteNumeric(raw, integer)) onChange(Number(raw));
    // Otherwise keep the keystroke locally and leave the config untouched.
  };
  return (
    <Field data-invalid={error || invalid ? true : undefined} data-testid={testId}>
      <FieldLabel>{label}</FieldLabel>
      <Input
        inputMode={integer ? "numeric" : "decimal"}
        value={text}
        onChange={(e) => apply(e.target.value)}
        disabled={pinned}
        aria-invalid={invalid || undefined}
      />
      {!pinned && <VariablePicker paths={variablePaths} onInsert={(p) => apply(`\${{ ${p} }}`)} />}
      {invalid && <FieldError>{integer ? "Enter a whole number" : "Enter a number"}</FieldError>}
      {error && <FieldError>{error}</FieldError>}
    </Field>
  );
}

function isFiniteNumeric(raw: string, integer: boolean): boolean {
  if (!/^-?\d+(\.\d+)?$/.test(raw.trim())) return false;
  const n = Number(raw);
  return Number.isFinite(n) && (!integer || Number.isInteger(n));
}

// ---------------------------------------------------------------------------
// Retry policy — shared by every non-system block
// ---------------------------------------------------------------------------

function RetryEditor({
  block,
  onChange,
  disabled,
}: {
  block: BlockDef;
  onChange: (b: BlockDef) => void;
  disabled: boolean;
}) {
  const retry = block.retry;
  return (
    <Field data-testid="field-retry">
      <FieldLabel className="flex items-center gap-1.5">
        Retry
        {disabled && (
          <span className="text-muted-foreground inline-flex items-center gap-1 text-xs font-normal">
            <Lock className="size-3" aria-hidden /> Set by the built-in
          </span>
        )}
      </FieldLabel>
      <div className="flex items-center gap-2">
        <Input
          type="number"
          min={1}
          max={5}
          className="w-20"
          value={retry?.attempts ?? 1}
          onChange={(e) => {
            const attempts = Number(e.target.value);
            if (!Number.isInteger(attempts) || attempts < 1) return;
            onChange({
              ...block,
              retry:
                attempts === 1 ? undefined : { attempts, retryOn: retry?.retryOn ?? "transient" },
            });
          }}
          disabled={disabled}
        />
        <span className="text-muted-foreground text-sm">attempts, retrying on</span>
        <Select
          value={retry?.retryOn ?? "transient"}
          onValueChange={(v) =>
            onChange({
              ...block,
              retry: {
                attempts: retry?.attempts ?? 2,
                retryOn: v as "transient" | "always" | "never",
              },
            })
          }
          disabled={disabled || !retry}
        >
          <SelectTrigger className="h-8 w-32">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="transient">transient errors</SelectItem>
            <SelectItem value="always">any error</SelectItem>
            <SelectItem value="never">never</SelectItem>
          </SelectContent>
        </Select>
      </div>
    </Field>
  );
}
