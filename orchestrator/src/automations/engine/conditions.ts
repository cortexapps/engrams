/** Structured condition evaluator (ADR 0119 D6).
 *
 * One evaluator serves the filter block, branch conditions, loop-until, and
 * trigger scope narrowing. The Code block is the escape hatch for anything
 * these rows cannot say. Regex work is bounded by the pattern and subject
 * caps AND by `safe-regex.ts`, which refuses the super-linear backtracking
 * shapes (nested quantifiers, backreferences) — length caps alone do not
 * bound backtracking, and there is no RE2 under Bun.
 */

import { isSafePath, jsonEqual, ownPath } from "../paths.ts";
import { assertSafeRegex, isSafeRegex, UnsafeRegexError } from "./safe-regex.ts";

export const CONDITION_MAX_DEPTH = 3;
export const CONDITION_MAX_LEAVES = 50;
export const CONDITION_PATTERN_MAX_CHARS = 256;
export const CONDITION_SUBJECT_MAX_CHARS = 4096;

export const FILTER_OPERATORS = [
  "equals",
  "not_equals",
  "contains",
  "matches",
  "in",
  "is_empty",
  "gt",
  "lt",
  "is_true",
  "is_false",
] as const;
export type FilterOperator = (typeof FILTER_OPERATORS)[number];

export interface FilterCondition {
  path: string;
  op: FilterOperator;
  value?: unknown;
}

export interface FilterGroup {
  mode: "all" | "any";
  conditions: Array<FilterCondition | FilterGroup>;
}

export class ConditionParseError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ConditionParseError";
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseCondition(raw: unknown, where: string): FilterCondition {
  if (!isRecord(raw)) throw new ConditionParseError(`${where}: condition must be an object`);
  const path = raw["path"];
  if (typeof path !== "string" || !isSafePath(path)) {
    throw new ConditionParseError(`${where}: invalid path`);
  }
  const op = raw["op"];
  if (typeof op !== "string" || !(FILTER_OPERATORS as readonly string[]).includes(op)) {
    throw new ConditionParseError(`${where}: unknown operator ${JSON.stringify(op)}`);
  }
  if (op === "matches") {
    const pattern = raw["value"];
    if (typeof pattern !== "string" || pattern.length > CONDITION_PATTERN_MAX_CHARS) {
      throw new ConditionParseError(`${where}: "matches" needs a string pattern (max ${CONDITION_PATTERN_MAX_CHARS} chars)`);
    }
    try {
      new RegExp(pattern, "u");
    } catch {
      throw new ConditionParseError(`${where}: invalid regular expression`);
    }
    try {
      assertSafeRegex(pattern);
    } catch (error) {
      if (!(error instanceof UnsafeRegexError)) throw error;
      throw new ConditionParseError(`${where}: ${error.message}`);
    }
  }
  if (op === "in" && !Array.isArray(raw["value"])) {
    throw new ConditionParseError(`${where}: "in" needs an array value`);
  }
  return { path, op: op as FilterOperator, value: raw["value"] };
}

/** Validate an untrusted group shape. Depth ≤ 3, ≤ 50 leaf conditions. */
export function parseFilterGroup(raw: unknown): FilterGroup {
  const leaves = { count: 0 };
  function walk(node: unknown, depth: number, where: string): FilterGroup {
    if (!isRecord(node)) throw new ConditionParseError(`${where}: group must be an object`);
    if (depth > CONDITION_MAX_DEPTH) {
      throw new ConditionParseError(`${where}: nesting deeper than ${CONDITION_MAX_DEPTH}`);
    }
    const mode = node["mode"];
    if (mode !== "all" && mode !== "any") {
      throw new ConditionParseError(`${where}: mode must be "all" or "any"`);
    }
    const rawConditions = node["conditions"];
    if (!Array.isArray(rawConditions)) {
      throw new ConditionParseError(`${where}: conditions must be an array`);
    }
    const conditions = rawConditions.map((child, i) => {
      const childWhere = `${where}.conditions[${i}]`;
      if (isRecord(child) && "mode" in child) return walk(child, depth + 1, childWhere);
      leaves.count += 1;
      if (leaves.count > CONDITION_MAX_LEAVES) {
        throw new ConditionParseError(`more than ${CONDITION_MAX_LEAVES} conditions`);
      }
      return parseCondition(child, childWhere);
    });
    return { mode, conditions };
  }
  return walk(raw, 1, "filter");
}

function isEmptyValue(value: unknown): boolean {
  if (value === undefined || value === null || value === "") return true;
  if (Array.isArray(value)) return value.length === 0;
  if (typeof value === "object") return Object.keys(value as object).length === 0;
  return false;
}

const NUMERIC_STRING = /^\s*[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?\s*$/;

/** `gt`/`lt` compare numbers, numeric strings as numbers, and everything
 * else that `Date.parse` accepts as epoch milliseconds. A numeric string
 * must be tried as a number FIRST: `Date.parse("2026")` is a year, and
 * `Date.parse("41")` is NaN — both wrong for a string-encoded count. */
function asComparable(value: unknown): number | undefined {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string") {
    if (NUMERIC_STRING.test(value)) {
      const num = Number(value);
      return Number.isFinite(num) ? num : undefined;
    }
    const parsed = Date.parse(value);
    if (!Number.isNaN(parsed)) return parsed;
  }
  return undefined;
}

function evaluateCondition(condition: FilterCondition, scope: Record<string, unknown>): boolean {
  const resolved = ownPath(scope, condition.path);
  switch (condition.op) {
    case "equals":
      return jsonEqual(resolved, condition.value);
    case "not_equals":
      return !jsonEqual(resolved, condition.value);
    case "contains":
      if (typeof resolved === "string" && typeof condition.value === "string") {
        return resolved.includes(condition.value);
      }
      if (Array.isArray(resolved)) return resolved.some((item) => jsonEqual(item, condition.value));
      return false;
    case "matches": {
      if (typeof resolved !== "string" || typeof condition.value !== "string") return false;
      // Fail closed on a pattern stored before the guard existed.
      if (!isSafeRegex(condition.value)) return false;
      const subject = resolved.slice(0, CONDITION_SUBJECT_MAX_CHARS);
      return new RegExp(condition.value, "u").test(subject);
    }
    case "in":
      return Array.isArray(condition.value) && condition.value.some((item) => jsonEqual(item, resolved));
    case "is_empty":
      return isEmptyValue(resolved);
    case "gt": {
      const a = asComparable(resolved);
      const b = asComparable(condition.value);
      return a !== undefined && b !== undefined && a > b;
    }
    case "lt": {
      const a = asComparable(resolved);
      const b = asComparable(condition.value);
      return a !== undefined && b !== undefined && a < b;
    }
    case "is_true":
      return resolved === true;
    case "is_false":
      return resolved === false;
  }
}

/** Evaluate a parsed group against a context scope. Missing paths resolve
 * `undefined`: every operator except `is_empty`/`not_equals` yields false. */
export function evaluateFilter(group: FilterGroup, scope: Record<string, unknown>): boolean {
  const results = group.conditions.map((child) =>
    "mode" in child ? evaluateFilter(child, scope) : evaluateCondition(child, scope),
  );
  if (group.mode === "all") return results.every(Boolean);
  return results.some(Boolean);
}
