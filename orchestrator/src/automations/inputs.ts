/** Automation input VALUE validation (ADR 0119 phase 4.3b).
 *
 * The single server-side guard on the values an automation runs with. It
 * mirrors the web's `lib/automation-inputs.ts` rule for rule (the two are
 * pinned to each other by a shared-fixture test), so an edit the Inputs tab
 * accepts is exactly what SetInputs accepts — and what the seeder and the
 * run snapshot accept. Three call sites:
 *
 *   - SetInputs / CreateAutomation / RunNow: reject with InvalidArgument,
 *     one `inputs.<key>[.path]: message` line per error (the format the
 *     web's inputErrorFromServer routes back to a field);
 *   - the built-in seeder: a bad legacy enrollment row falls back to the
 *     schema default for that key instead of blocking boot;
 *   - the run snapshot: stored values that a (possibly newer) version's
 *     schema rejects fail the run BEFORE the walk, never inside it.
 */

import type { InputFieldSpec } from "./engine/definition.ts";

export interface InputValueError {
  key: string;
  /** Dotted path under the field for map/list errors ("engrams/engrams.mode", "0"). */
  path?: string;
  code: "required" | "type" | "enum" | "map_key" | "shape";
  message: string;
}

type ScalarType = "string" | "number" | "boolean" | "enum";

interface ValueFieldSpec {
  key: string;
  type: ScalarType;
  values?: string[];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The object fields of a map value (from `valueShape`) — same projection as
 * the web's mapValueFields: only scalar-typed entries are modelled. */
export function mapValueFields(spec: InputFieldSpec): ValueFieldSpec[] {
  const shape = spec.valueShape;
  if (!shape) return [];
  const out: ValueFieldSpec[] = [];
  for (const [key, raw] of Object.entries(shape)) {
    if (!isRecord(raw)) continue;
    const type = raw["type"];
    if (type !== "string" && type !== "number" && type !== "boolean" && type !== "enum") continue;
    const field: ValueFieldSpec = { key, type };
    if (Array.isArray(raw["values"])) {
      field.values = raw["values"].filter((v): v is string => typeof v === "string");
    }
    out.push(field);
  }
  return out;
}

/** The element spec of a list input (`valueShape.element`), defaulting to a
 * free string. */
export function listElementField(spec: InputFieldSpec): ValueFieldSpec {
  const element = spec.valueShape?.["element"];
  if (isRecord(element)) {
    const parsed = mapValueFields({ ...spec, valueShape: { element } });
    if (parsed[0]) return { ...parsed[0], key: "element" };
  }
  return { key: "element", type: "string" };
}

const MAP_KEY_RE: Record<NonNullable<InputFieldSpec["keyNoun"]>, RegExp> = {
  // owner/repo, GitHub's own character set.
  repository: /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/,
  // Slack channel ids (C…/G…/D…) or a #name.
  channel: /^(?:[CGD][A-Z0-9]{8,}|#[a-z0-9_-]+)$/,
  // Linear team keys are short uppercase identifiers.
  team: /^[A-Z][A-Z0-9]{1,9}$/,
};

export function isValidMapKey(noun: InputFieldSpec["keyNoun"], key: string): boolean {
  if (!key.trim()) return false;
  if (!noun) return true;
  return MAP_KEY_RE[noun].test(key.trim());
}

export function mapKeyHint(noun: InputFieldSpec["keyNoun"]): string {
  switch (noun) {
    case "repository":
      return "owner/repo";
    case "channel":
      return "channel id (C…) or #name";
    case "team":
      return "team key (e.g. ENG)";
    default:
      return "key";
  }
}

function validateScalar(
  field: ValueFieldSpec,
  value: unknown,
  key: string,
  path: string,
  errors: InputValueError[],
): void {
  switch (field.type) {
    case "string":
      if (typeof value !== "string") errors.push({ key, path, code: "type", message: "must be text" });
      break;
    case "number":
      if (typeof value !== "number" || !Number.isFinite(value)) {
        errors.push({ key, path, code: "type", message: "must be a number" });
      }
      break;
    case "boolean":
      if (typeof value !== "boolean") {
        errors.push({ key, path, code: "type", message: "must be on or off" });
      }
      break;
    case "enum":
      if (typeof value !== "string" || !(field.values ?? []).includes(value)) {
        errors.push({
          key,
          path,
          code: "enum",
          message: `must be one of ${(field.values ?? []).join(", ")}`,
        });
      }
      break;
  }
}

function isEmpty(value: unknown): boolean {
  return (
    value === undefined ||
    value === null ||
    value === "" ||
    (Array.isArray(value) && value.length === 0) ||
    (isRecord(value) && Object.keys(value).length === 0)
  );
}

/** Validate stored/submitted values against a version's inputs schema.
 * Returns every violation (never throws); an empty list means valid. Keys
 * absent from the schema are NOT reported here — undeclared-key rejection
 * stays with the RPC, which owns that policy. */
export function validateInputValues(
  schema: readonly InputFieldSpec[],
  values: Record<string, unknown>,
): InputValueError[] {
  const errors: InputValueError[] = [];
  for (const spec of schema) {
    const value = values[spec.key];
    const empty = isEmpty(value);
    if (spec.required && empty) {
      errors.push({ key: spec.key, code: "required", message: `${spec.label} is required` });
      continue;
    }
    if (empty) continue;
    switch (spec.type) {
      case "string":
      case "secret_ref":
        if (typeof value !== "string") {
          errors.push({ key: spec.key, code: "type", message: "must be text" });
        }
        break;
      case "number":
        if (typeof value !== "number" || !Number.isFinite(value)) {
          errors.push({ key: spec.key, code: "type", message: "must be a number" });
        }
        break;
      case "boolean":
        if (typeof value !== "boolean") {
          errors.push({ key: spec.key, code: "type", message: "must be on or off" });
        }
        break;
      case "enum":
        if (typeof value !== "string" || !(spec.values ?? []).includes(value)) {
          errors.push({
            key: spec.key,
            code: "enum",
            message: `must be one of ${(spec.values ?? []).join(", ")}`,
          });
        }
        break;
      case "list": {
        if (!Array.isArray(value)) {
          errors.push({ key: spec.key, code: "type", message: "must be a list" });
          break;
        }
        const element = listElementField(spec);
        value.forEach((item, i) => validateScalar(element, item, spec.key, String(i), errors));
        break;
      }
      case "map": {
        if (!isRecord(value)) {
          errors.push({ key: spec.key, code: "type", message: "must be a map" });
          break;
        }
        const fields = mapValueFields(spec);
        for (const [rowKey, row] of Object.entries(value)) {
          if (!isValidMapKey(spec.keyNoun, rowKey)) {
            errors.push({
              key: spec.key,
              path: rowKey,
              code: "map_key",
              message: `not a valid ${mapKeyHint(spec.keyNoun)}`,
            });
            continue;
          }
          if (!isRecord(row)) {
            errors.push({ key: spec.key, path: rowKey, code: "shape", message: "row must be an object" });
            continue;
          }
          for (const field of fields) {
            validateScalar(field, row[field.key], spec.key, `${rowKey}.${field.key}`, errors);
          }
        }
        break;
      }
      case "json":
        if (!isRecord(value)) {
          errors.push({ key: spec.key, code: "type", message: "must be a JSON object" });
        }
        break;
    }
  }
  return errors;
}

/** The routed-line form the web parses: `inputs.<key>[.path]: message`. */
export function inputErrorField(error: InputValueError): string {
  return error.path !== undefined ? `inputs.${error.key}.${error.path}` : `inputs.${error.key}`;
}

/** Raised by the run snapshot when stored inputs fail the pinned schema. */
export class InputValidationError extends Error {
  constructor(readonly errors: InputValueError[]) {
    super(
      `inputs rejected by the automation's schema: ${errors
        .map((e) => `${inputErrorField(e)}: ${e.message}`)
        .join("; ")}`,
    );
    this.name = "InputValidationError";
  }
}
