/** Runtime validation of values against the bounded FieldSchema subset
 * (ADR 0119 D5). The registry validates the SCHEMAS at load; this validates
 * VALUES against them — action params before execute, and later the editor's
 * typed field picker. Collects every violation instead of stopping at the
 * first, so a bad block config surfaces completely in one pass.
 */

import type { FieldSchema } from "./registry.ts";

export interface FieldValueError {
  /** Dot path from the root ("" for the root itself). */
  path: string;
  message: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function walk(
  schema: FieldSchema,
  value: unknown,
  path: string,
  errors: FieldValueError[],
): void {
  switch (schema.type) {
    case "object": {
      if (!isRecord(value)) {
        errors.push({ path, message: "expected an object" });
        return;
      }
      for (const required of schema.required ?? []) {
        if (value[required] === undefined) {
          errors.push({ path: path === "" ? required : `${path}.${required}`, message: "required" });
        }
      }
      for (const [key, child] of Object.entries(value)) {
        const childSchema = schema.properties[key];
        const childPath = path === "" ? key : `${path}.${key}`;
        if (childSchema === undefined) {
          errors.push({ path: childPath, message: "unknown property" });
          continue;
        }
        if (child !== undefined) walk(childSchema, child, childPath, errors);
      }
      return;
    }
    case "array": {
      if (!Array.isArray(value)) {
        errors.push({ path, message: "expected an array" });
        return;
      }
      if (schema.items !== undefined) {
        value.forEach((item, i) => walk(schema.items!, item, `${path}[${i}]`, errors));
      }
      return;
    }
    case "string": {
      if (typeof value !== "string") {
        errors.push({ path, message: "expected a string" });
        return;
      }
      if (schema.enum !== undefined && !schema.enum.includes(value)) {
        errors.push({ path, message: `expected one of ${schema.enum.join(", ")}` });
      }
      return;
    }
    case "number": {
      if (typeof value !== "number" || !Number.isFinite(value)) {
        errors.push({ path, message: "expected a number" });
      }
      return;
    }
    case "integer": {
      if (typeof value !== "number" || !Number.isInteger(value)) {
        errors.push({ path, message: "expected an integer" });
      }
      return;
    }
    case "boolean": {
      if (typeof value !== "boolean") {
        errors.push({ path, message: "expected a boolean" });
      }
      return;
    }
  }
}

const INTEGER_RE = /^-?\d+$/;
const NUMBER_RE = /^-?(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?$/;

/** Coerce templated scalars to the schema's type. A Liquid template ALWAYS
 * renders a string, so `number: "${{ event.raw.pull_request.number }}"`
 * arrives as "1539" for an `integer` param; every author who templates a
 * numeric or boolean param would otherwise hit "expected an integer" at
 * run time (the PR-review built-in did, on its first live pull request).
 * Only an unambiguous string converts: an integer literal for `integer`, a
 * numeric literal for `number`, `true`/`false` for `boolean`. Anything else
 * passes through untouched for `validateFieldValue` to reject. Objects and
 * arrays recurse; unknown properties are left for validation to flag. */
export function coerceFieldValue(schema: FieldSchema, value: unknown): unknown {
  switch (schema.type) {
    case "object": {
      if (!isRecord(value)) return value;
      const out: Record<string, unknown> = {};
      for (const [key, child] of Object.entries(value)) {
        const childSchema = schema.properties[key];
        out[key] = childSchema === undefined ? child : coerceFieldValue(childSchema, child);
      }
      return out;
    }
    case "array":
      if (!Array.isArray(value) || schema.items === undefined) return value;
      return value.map((item) => coerceFieldValue(schema.items!, item));
    case "integer":
      return typeof value === "string" && INTEGER_RE.test(value.trim()) ? Number(value.trim()) : value;
    case "number":
      return typeof value === "string" && NUMBER_RE.test(value.trim()) ? Number(value.trim()) : value;
    case "boolean":
      return value === "true" ? true : value === "false" ? false : value;
    case "string":
      return value;
  }
}

/** Validate a value against a FieldSchema. Unknown object properties are
 * violations — action inputs are a closed contract. */
export function validateFieldValue(schema: FieldSchema, value: unknown): FieldValueError[] {
  const errors: FieldValueError[] = [];
  walk(schema, value, "", errors);
  return errors;
}
