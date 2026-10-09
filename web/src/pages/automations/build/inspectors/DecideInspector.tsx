/** The decide block's inspector: the state the model judges, and a list of
 * typed questions (choice / score / yes_no) answered in one call.
 *
 * A choice question takes its options either inline (value, label,
 * description rows) or from a variable — `{ "$ref": "steps.x.options" }`,
 * which is how a list_profiles block feeds profile ids in. */

import { Plus, Trash2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Field, FieldDescription, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import {
  isTunable,
  setPath,
  type BlockDef,
  type BlockErrorRef,
  type FieldSpec,
} from "@/lib/automation-blocks";

import { GenericField } from "../fields/GenericField";

export interface DecideInspectorProps {
  block: BlockDef;
  onChange: (next: BlockDef) => void;
  builtin: boolean;
  errors: readonly BlockErrorRef[];
  sessionSources: readonly string[];
  variablePaths: readonly string[];
  variableValues?: Readonly<Record<string, string>>;
}

type QuestionType = "choice" | "score" | "yes_no";
const QUESTION_TYPES: readonly QuestionType[] = ["choice", "score", "yes_no"];
const QUESTION_TYPE_LABEL: Record<QuestionType, string> = {
  choice: "Choice — pick one option",
  score: "Score — rate on ordered levels",
  yes_no: "Yes / no — probability of yes",
};

interface ChoiceOptionRow {
  value: string;
  label?: string;
  description?: string;
}

type Question = Record<string, unknown> & { type?: QuestionType; instructions?: unknown };

function errorFor(errors: readonly BlockErrorRef[], field: string): string | undefined {
  return errors.find((e) => e.field === field || e.field.startsWith(`${field}.`))?.message;
}

function str(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function refPath(value: unknown): string | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const ref = (value as { $ref?: unknown }).$ref;
  return typeof ref === "string" ? ref : null;
}

/** A fresh question of `type`, keeping the instructions across a type change. */
function freshQuestion(type: QuestionType, instructions: unknown = ""): Question {
  switch (type) {
    case "choice":
      return { type, instructions, options: [{ value: "" }] };
    case "score":
      return { type, instructions, levels: ["low", "high"] };
    case "yes_no":
      return { type, instructions };
  }
}

/** The next unused question id (`q1`, `q2`, …). */
function nextQuestionId(questions: Record<string, unknown>): string {
  for (let i = 1; ; i += 1) {
    if (!(`q${i}` in questions)) return `q${i}`;
  }
}

const STATE_FIELD: FieldSpec = {
  type: "template",
  key: "state",
  label: "State",
  multiline: true,
  help: "What the questions are about — the message, the alert, the diff summary.",
};
const STATE_JSON_FIELD: FieldSpec = {
  type: "json",
  key: "state",
  label: "State (structured)",
  help: 'A JSON object, or {"$ref": "steps.x.y"} for an earlier block\'s output.',
};
const MODEL_FIELD: FieldSpec = {
  type: "string",
  key: "model",
  label: "Model",
  help: "Default: typesafe/jev-1.13 (pinned, so thresholds stay stable).",
};
const ON_ERROR_FIELD: FieldSpec = {
  type: "select",
  key: "onError",
  label: "When the call fails",
  options: ["fail", "undecided"],
  help: "undecided returns decided: false so the graph can fall back instead of failing.",
};

export function DecideInspector(props: DecideInspectorProps) {
  const { block, onChange, builtin, errors, sessionSources, variablePaths, variableValues } = props;
  const pinned = (key: string) => builtin && !isTunable(block, key);
  const update = (key: string, value: unknown) =>
    onChange({ ...block, config: setPath(block.config, key, value) });
  const state = block.config["state"];
  const structured = typeof state !== "string" && state !== undefined;
  const questions = (
    typeof block.config["questions"] === "object" && block.config["questions"] !== null
      ? block.config["questions"]
      : {}
  ) as Record<string, Question>;
  const questionsPinned = pinned("questions");

  const setQuestions = (next: Record<string, Question>) => update("questions", next);
  const renameQuestion = (from: string, to: string) => {
    if (to === from || to in questions) return;
    const next: Record<string, Question> = {};
    for (const [id, q] of Object.entries(questions)) next[id === from ? to : id] = q;
    setQuestions(next);
  };

  const field = (spec: FieldSpec) => (
    <GenericField
      key={spec.key}
      spec={spec}
      value={block.config[spec.key]}
      onChange={(value) => update(spec.key, value)}
      pinned={pinned(spec.key)}
      error={errorFor(errors, spec.key)}
      sessionSources={sessionSources}
      variablePaths={variablePaths}
      variableValues={variableValues}
    />
  );

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-end">
        <Button
          type="button"
          variant="ghost"
          size="sm"
          disabled={pinned("state")}
          onClick={() => update("state", structured ? "" : {})}
        >
          {structured ? "Use text state" : "Use structured state"}
        </Button>
      </div>
      {field(structured ? STATE_JSON_FIELD : STATE_FIELD)}

      <div className="flex flex-col gap-2">
        <div className="flex items-center justify-between">
          <span className="text-sm font-medium">Questions</span>
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={questionsPinned}
            onClick={() =>
              setQuestions({ ...questions, [nextQuestionId(questions)]: freshQuestion("yes_no") })
            }
          >
            <Plus className="size-4" aria-hidden />
            Add question
          </Button>
        </div>
        <p className="text-muted-foreground text-xs">
          Every question is answered in one call. Read an answer as{" "}
          <code>steps.{block.id}.answers.&lt;id&gt;</code> — e.g. <code>.value</code> and{" "}
          <code>.confidence</code> for a choice, <code>.yes</code> for yes / no.
        </p>
        {errorFor(errors, "questions") && <FieldError>{errorFor(errors, "questions")}</FieldError>}
        {Object.entries(questions).map(([id, question]) => (
          <QuestionCard
            key={id}
            id={id}
            question={question}
            disabled={questionsPinned}
            onRename={(to) => renameQuestion(id, to)}
            onChange={(next) => setQuestions({ ...questions, [id]: next })}
            onRemove={() => {
              const next = { ...questions };
              delete next[id];
              setQuestions(next);
            }}
          />
        ))}
      </div>

      {field(MODEL_FIELD)}
      {field(ON_ERROR_FIELD)}
    </div>
  );
}

function QuestionCard({
  id,
  question,
  disabled,
  onRename,
  onChange,
  onRemove,
}: {
  id: string;
  question: Question;
  disabled: boolean;
  onRename: (to: string) => void;
  onChange: (next: Question) => void;
  onRemove: () => void;
}) {
  const type: QuestionType = QUESTION_TYPES.includes(question.type as QuestionType)
    ? (question.type as QuestionType)
    : "yes_no";
  return (
    <div className="flex flex-col gap-3 rounded-md border p-3" data-testid={`question-${id}`}>
      <div className="flex gap-2">
        <Input
          aria-label="Question id"
          className="w-32 font-mono text-sm"
          defaultValue={id}
          disabled={disabled}
          onBlur={(e) => {
            const to = e.target.value.trim();
            if (/^[a-z][a-z0-9_]*$/.test(to)) onRename(to);
            else e.target.value = id;
          }}
        />
        <Select
          value={type}
          disabled={disabled}
          onValueChange={(next) =>
            onChange(freshQuestion(next as QuestionType, question.instructions))
          }
        >
          <SelectTrigger className="flex-1" aria-label="Question type">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {QUESTION_TYPES.map((t) => (
              <SelectItem key={t} value={t}>
                {QUESTION_TYPE_LABEL[t]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          aria-label={`Remove question ${id}`}
          disabled={disabled}
          onClick={onRemove}
        >
          <Trash2 className="size-4" aria-hidden />
        </Button>
      </div>
      <Field>
        <FieldLabel>Question</FieldLabel>
        <Textarea
          rows={2}
          value={str(question.instructions)}
          disabled={disabled}
          placeholder="Ask one specific thing."
          onChange={(e) => onChange({ ...question, instructions: e.target.value })}
        />
      </Field>
      {type === "choice" && (
        <ChoiceOptions question={question} disabled={disabled} onChange={onChange} />
      )}
      {type === "score" && (
        <ScoreLevels question={question} disabled={disabled} onChange={onChange} />
      )}
      {type === "yes_no" && (
        <div className="grid grid-cols-2 gap-2">
          {(["yes", "no"] as const).map((side) => (
            <Field key={side}>
              <FieldLabel>“{side}” means</FieldLabel>
              <Input
                value={str(question[side])}
                disabled={disabled}
                placeholder="optional"
                onChange={(e) => {
                  const next: Question = { ...question };
                  if (e.target.value === "") delete next[side];
                  else next[side] = e.target.value;
                  onChange(next);
                }}
              />
            </Field>
          ))}
          <FieldDescription className="col-span-2">Set both sides or neither.</FieldDescription>
        </div>
      )}
    </div>
  );
}

function ChoiceOptions({
  question,
  disabled,
  onChange,
}: {
  question: Question;
  disabled: boolean;
  onChange: (next: Question) => void;
}) {
  const ref = refPath(question["options"]);
  const rows = (Array.isArray(question["options"]) ? question["options"] : []) as ChoiceOptionRow[];
  const setRows = (next: ChoiceOptionRow[]) => onChange({ ...question, options: next });
  const setRow = (index: number, patch: Partial<ChoiceOptionRow>) =>
    setRows(
      rows.map((row, i) => {
        if (i !== index) return row;
        const merged: ChoiceOptionRow = { ...row, ...patch };
        if (!merged.label) delete merged.label;
        if (!merged.description) delete merged.description;
        return merged;
      }),
    );
  return (
    <Field>
      <div className="flex items-center justify-between">
        <FieldLabel>Options</FieldLabel>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          disabled={disabled}
          onClick={() =>
            onChange({ ...question, options: ref === null ? { $ref: "" } : [{ value: "" }] })
          }
        >
          {ref === null ? "From a variable" : "List them here"}
        </Button>
      </div>
      {ref !== null ? (
        <>
          <Input
            className="font-mono text-sm"
            value={ref}
            disabled={disabled}
            placeholder="steps.candidates.options"
            onChange={(e) => onChange({ ...question, options: { $ref: e.target.value } })}
          />
          <FieldDescription>
            A list of {"{ value, label?, description? }"} — e.g. a List profiles block's options.
          </FieldDescription>
        </>
      ) : (
        <>
          {rows.map((row, index) => (
            <div key={index} className="flex flex-col gap-1 rounded border p-2">
              <div className="flex gap-2">
                <Input
                  aria-label="Option value"
                  className="font-mono text-sm"
                  placeholder="value"
                  value={row.value}
                  disabled={disabled}
                  onChange={(e) => setRow(index, { value: e.target.value })}
                />
                <Input
                  aria-label="Option label"
                  placeholder="label (default: value)"
                  value={row.label ?? ""}
                  disabled={disabled}
                  onChange={(e) => setRow(index, { label: e.target.value })}
                />
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  aria-label="Remove option"
                  disabled={disabled || rows.length <= 1}
                  onClick={() => setRows(rows.filter((_, i) => i !== index))}
                >
                  <Trash2 className="size-4" aria-hidden />
                </Button>
              </div>
              <Input
                aria-label="Option description"
                placeholder="What this option covers (tell similar options apart)"
                value={typeof row.description === "string" ? row.description : ""}
                disabled={disabled}
                onChange={(e) => setRow(index, { description: e.target.value })}
              />
            </div>
          ))}
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={disabled || rows.length >= 255}
            onClick={() => setRows([...rows, { value: "" }])}
          >
            <Plus className="size-4" aria-hidden />
            Add option
          </Button>
          <FieldDescription>
            Add a catch-all (“other”) when the list may not cover every case.
          </FieldDescription>
        </>
      )}
    </Field>
  );
}

function ScoreLevels({
  question,
  disabled,
  onChange,
}: {
  question: Question;
  disabled: boolean;
  onChange: (next: Question) => void;
}) {
  const levels = (Array.isArray(question["levels"]) ? question["levels"] : []).map(str);
  const setLevels = (next: string[]) => onChange({ ...question, levels: next });
  return (
    <Field>
      <FieldLabel>Levels (lowest first)</FieldLabel>
      {levels.map((level, index) => (
        <div key={index} className="flex gap-2">
          <span className="text-muted-foreground w-5 pt-2 text-xs">{index}</span>
          <Input
            aria-label={`Level ${index}`}
            value={level}
            disabled={disabled}
            onChange={(e) => setLevels(levels.map((l, i) => (i === index ? e.target.value : l)))}
          />
          <Button
            type="button"
            variant="ghost"
            size="icon"
            aria-label={`Remove level ${index}`}
            disabled={disabled || levels.length <= 2}
            onClick={() => setLevels(levels.filter((_, i) => i !== index))}
          >
            <Trash2 className="size-4" aria-hidden />
          </Button>
        </div>
      ))}
      <Button
        type="button"
        variant="outline"
        size="sm"
        disabled={disabled || levels.length >= 10}
        onClick={() => setLevels([...levels, ""])}
      >
        <Plus className="size-4" aria-hidden />
        Add level
      </Button>
    </Field>
  );
}
