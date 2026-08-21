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

/** Validate a value against a FieldSchema. Unknown object properties are
 * violations — action inputs are a closed contract. */
export function validateFieldValue(schema: FieldSchema, value: unknown): FieldValueError[] {
  const errors: FieldValueError[] = [];
  walk(schema, value, "", errors);
  return errors;
}
