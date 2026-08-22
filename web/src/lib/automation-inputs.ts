/** Automation inputs: schema parsing, defaults, validation, and the payload
 * (ADR 0119 built-in editing model — the Inputs tab is editable on every
 * automation, built-ins included).
 *
 * The schema is the `inputsSchema` array of the automation's current
 * version; the values are `inputs_json` on the automation row.
 *
 * Validation here is a courtesy for the form: the orchestrator re-validates
 * VALUES on SetInputs/CreateAutomation/RunNow with the same rules
 * (orchestrator/src/automations/inputs.ts — the two are pinned to each
 * other by a shared-fixture test) and answers in `inputs.<key>[.path]:
 * message` lines that inputErrorFromServer routes back to the field. The
 * seeder and the run snapshot apply the same rules, so nothing reaches a
 * running automation that this form would have refused. */

export const INPUT_TYPES = [
  "string",
  "number",
  "boolean",
  "enum",
  "secret_ref",
  "list",
  "map",
  "json",
] as const;
export type InputType = (typeof INPUT_TYPES)[number];

export const KEY_NOUNS = ["repository", "channel", "team"] as const;
export type KeyNoun = (typeof KEY_NOUNS)[number];

/** One field of a map value / list element (the `valueShape` of a map or
 * list input). Only the shapes the built-ins use are modelled: a flat object
 * of scalar-typed fields, or a scalar element. */
export interface ValueFieldSpec {
  key: string;
  label: string;
  type: "string" | "number" | "boolean" | "enum";
  values?: string[];
  default?: unknown;
}

export interface InputFieldSpec {
  key: string;
  label: string;
  type: InputType;
  help?: string;
  required?: boolean;
  default?: unknown;
  /** enum: allowed values. */
  values?: string[];
  /** string: render a textarea. */
  multiline?: boolean;
  /** map: the integration noun that populates the key picker. */
  keyNoun?: KeyNoun;
  /** map: the object fields of each value; list: `{ element: ValueFieldSpec }`. */
  valueShape?: Record<string, unknown>;
}

export type InputValues = Record<string, unknown>;

export interface InputFieldError {
  key: string;
  /** Dotted path under the field for map/list errors (e.g. "engrams/engrams.mode"). */
  path?: string;
  message: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isInputType(value: unknown): value is InputType {
  return typeof value === "string" && (INPUT_TYPES as readonly string[]).includes(value);
}

function isKeyNoun(value: unknown): value is KeyNoun {
  return typeof value === "string" && (KEY_NOUNS as readonly string[]).includes(value);
}

/** Parse an untrusted `inputsSchema` array; malformed entries are dropped so
 * a bad server row degrades to fewer fields, never a crash. */
export function parseInputsSchema(raw: unknown): InputFieldSpec[] {
  if (!Array.isArray(raw)) return [];
  const out: InputFieldSpec[] = [];
  for (const entry of raw) {
    if (!isRecord(entry)) continue;
    const key = entry["key"];
    const type = entry["type"];
    if (typeof key !== "string" || !key || !isInputType(type)) continue;
    const spec: InputFieldSpec = {
      key,
      label: typeof entry["label"] === "string" && entry["label"] ? entry["label"] : key,
      type,
    };
    if (typeof entry["help"] === "string") spec.help = entry["help"];
    if (entry["required"] === true) spec.required = true;
    if ("default" in entry) spec.default = entry["default"];
    if (Array.isArray(entry["values"])) {
      spec.values = entry["values"].filter((v): v is string => typeof v === "string");
    }
    if (entry["multiline"] === true) spec.multiline = true;
    if (isKeyNoun(entry["keyNoun"])) spec.keyNoun = entry["keyNoun"];
    if (isRecord(entry["valueShape"])) spec.valueShape = entry["valueShape"];
    out.push(spec);
  }
  return out;
}

/** The object fields of a map input's value (from `valueShape`). */
export function mapValueFields(spec: InputFieldSpec): ValueFieldSpec[] {
  const shape = spec.valueShape;
  if (!shape) return [];
  const out: ValueFieldSpec[] = [];
  for (const [key, raw] of Object.entries(shape)) {
    if (!isRecord(raw)) continue;
    const type = raw["type"];
    if (type !== "string" && type !== "number" && type !== "boolean" && type !== "enum") continue;
    const field: ValueFieldSpec = {
      key,
      label: typeof raw["label"] === "string" && raw["label"] ? raw["label"] : key,
      type,
    };
    if (Array.isArray(raw["values"])) {
      field.values = raw["values"].filter((v): v is string => typeof v === "string");
    }
    if ("default" in raw) field.default = raw["default"];
    out.push(field);
  }
  return out;
}

/** The element spec of a list input (from `valueShape.element`), defaulting
 * to a free string. */
export function listElementField(spec: InputFieldSpec): ValueFieldSpec {
  const element = spec.valueShape?.["element"];
  if (isRecord(element)) {
    const parsed = mapValueFields({ ...spec, valueShape: { element } });
    if (parsed[0]) return { ...parsed[0], key: "element", label: spec.label };
  }
  return { key: "element", label: spec.label, type: "string" };
}

function scalarDefault(field: ValueFieldSpec): unknown {
  if ("default" in field) return field.default;
  switch (field.type) {
    case "boolean":
      return false;
    case "number":
      return 0;
    case "enum":
      return field.values?.[0] ?? "";
    default:
      return "";
  }
}

/** A fresh value for one map row (every value field at its default). */
export function defaultMapRow(spec: InputFieldSpec): Record<string, unknown> {
  const row: Record<string, unknown> = {};
  for (const field of mapValueFields(spec)) row[field.key] = scalarDefault(field);
  return row;
}

/** Schema defaults ⊕ stored values. Stored values win field by field; a
 * stored value of the wrong shape is ignored in favor of the default. */
export function resolveInputValues(schema: InputFieldSpec[], stored: unknown): InputValues {
  const storedRecord = isRecord(stored) ? stored : {};
  const out: InputValues = {};
  for (const spec of schema) {
    const value = storedRecord[spec.key];
    out[spec.key] = value !== undefined && shapeMatches(spec, value) ? value : fallback(spec);
  }
  return out;
}

function fallback(spec: InputFieldSpec): unknown {
  if ("default" in spec && spec.default !== undefined) return spec.default;
  switch (spec.type) {
    case "boolean":
      return false;
    case "number":
      return 0;
    case "enum":
      return spec.values?.[0] ?? "";
    case "list":
      return [];
    case "map":
    case "json":
      return {};
    default:
      return "";
  }
}

function shapeMatches(spec: InputFieldSpec, value: unknown): boolean {
  switch (spec.type) {
    case "string":
    case "secret_ref":
    case "enum":
      return typeof value === "string";
    case "number":
      return typeof value === "number";
    case "boolean":
      return typeof value === "boolean";
    case "list":
      return Array.isArray(value);
    case "map":
    case "json":
      return isRecord(value);
  }
}

export function parseInputsJson(json: string | undefined): unknown {
  if (!json) return {};
  try {
    return JSON.parse(json);
  } catch {
    return {};
  }
}

function validateScalar(
  field: ValueFieldSpec,
  value: unknown,
  key: string,
  path: string,
  errors: InputFieldError[],
): void {
  switch (field.type) {
    case "string":
      if (typeof value !== "string") errors.push({ key, path, message: "must be text" });
      break;
    case "number":
      if (typeof value !== "number" || !Number.isFinite(value)) {
        errors.push({ key, path, message: "must be a number" });
      }
      break;
    case "boolean":
      if (typeof value !== "boolean") errors.push({ key, path, message: "must be on or off" });
      break;
    case "enum":
      if (typeof value !== "string" || !(field.values ?? []).includes(value)) {
        errors.push({
          key,
          path,
          message: `must be one of ${(field.values ?? []).join(", ")}`,
        });
      }
      break;
  }
}

const MAP_KEY_RE: Record<KeyNoun, RegExp> = {
  // owner/repo, GitHub's own character set.
  repository: /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/,
  // Slack channel ids (C…/G…/D…) or a #name.
  channel: /^(?:[CGD][A-Z0-9]{8,}|#[a-z0-9_-]+)$/,
  // Linear team keys are short uppercase identifiers.
  team: /^[A-Z][A-Z0-9]{1,9}$/,
};

export function isValidMapKey(noun: KeyNoun | undefined, key: string): boolean {
  if (!key.trim()) return false;
  if (!noun) return true;
  return MAP_KEY_RE[noun].test(key.trim());
}

export function mapKeyHint(noun: KeyNoun | undefined): string {
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

/** Normalize a free-typed key for its noun (repos are case-insensitive on
 * the matching side; keep what the user typed otherwise). */
export function normalizeMapKey(noun: KeyNoun | undefined, key: string): string {
  const trimmed = key.trim();
  return noun === "repository" ? trimmed.toLowerCase() : trimmed;
}

/** Client-side validation mirroring what the server enforces. */
export function validateInputs(schema: InputFieldSpec[], values: InputValues): InputFieldError[] {
  const errors: InputFieldError[] = [];
  for (const spec of schema) {
    const value = values[spec.key];
    const empty =
      value === undefined ||
      value === null ||
      value === "" ||
      (Array.isArray(value) && value.length === 0) ||
      (isRecord(value) && Object.keys(value).length === 0);
    if (spec.required && empty) {
      errors.push({ key: spec.key, message: `${spec.label} is required` });
      continue;
    }
    if (empty) continue;
    switch (spec.type) {
      case "string":
      case "secret_ref":
        if (typeof value !== "string") errors.push({ key: spec.key, message: "must be text" });
        break;
      case "number":
        if (typeof value !== "number" || !Number.isFinite(value)) {
          errors.push({ key: spec.key, message: "must be a number" });
        }
        break;
      case "boolean":
        if (typeof value !== "boolean")
          errors.push({ key: spec.key, message: "must be on or off" });
        break;
      case "enum":
        if (typeof value !== "string" || !(spec.values ?? []).includes(value)) {
          errors.push({
            key: spec.key,
            message: `must be one of ${(spec.values ?? []).join(", ")}`,
          });
        }
        break;
      case "list": {
        if (!Array.isArray(value)) {
          errors.push({ key: spec.key, message: "must be a list" });
          break;
        }
        const element = listElementField(spec);
        value.forEach((item, i) => validateScalar(element, item, spec.key, String(i), errors));
        break;
      }
      case "map": {
        if (!isRecord(value)) {
          errors.push({ key: spec.key, message: "must be a map" });
          break;
        }
        const fields = mapValueFields(spec);
        for (const [rowKey, row] of Object.entries(value)) {
          if (!isValidMapKey(spec.keyNoun, rowKey)) {
            errors.push({
              key: spec.key,
              path: rowKey,
              message: `not a valid ${mapKeyHint(spec.keyNoun)}`,
            });
            continue;
          }
          if (!isRecord(row)) {
            errors.push({ key: spec.key, path: rowKey, message: "row must be an object" });
            continue;
          }
          for (const field of fields) {
            validateScalar(field, row[field.key], spec.key, `${rowKey}.${field.key}`, errors);
          }
        }
        break;
      }
      case "json":
        if (!isRecord(value)) errors.push({ key: spec.key, message: "must be a JSON object" });
        break;
    }
  }
  return errors;
}

/** The `inputs_json` payload for SetInputs: only schema keys, no strays. */
export function buildInputsPayload(schema: InputFieldSpec[], values: InputValues): string {
  const out: InputValues = {};
  for (const spec of schema) {
    if (spec.key in values) out[spec.key] = values[spec.key];
  }
  return JSON.stringify(out);
}

/** Route a server-side error (BlockError with field "inputs.<key>[.path]"
 * or a bare "<key>") back to a schema field. */
export function inputErrorFromServer(
  field: string,
  message: string,
  schema: InputFieldSpec[],
): InputFieldError | null {
  const stripped = field.startsWith("inputs.") ? field.slice("inputs.".length) : field;
  const [key, ...rest] = stripped.split(".");
  if (!key || !schema.some((spec) => spec.key === key)) return null;
  return rest.length > 0 ? { key, path: rest.join("."), message } : { key, message };
}

/** Structural equality for dirty-state tracking. */
export function inputsEqual(a: InputValues, b: InputValues): boolean {
  return JSON.stringify(sortKeys(a)) === JSON.stringify(sortKeys(b));
}

function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (!isRecord(value)) return value;
  const out: Record<string, unknown> = {};
  for (const key of Object.keys(value).sort()) out[key] = sortKeys(value[key]);
  return out;
}
