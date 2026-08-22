import { describe, expect, test } from "bun:test";

import {
  ConditionParseError,
  type FilterGroup,
  evaluateFilter,
  parseFilterGroup,
  CONDITION_MAX_LEAVES,
} from "../conditions.ts";

const scope = {
  event: {
    action: "opened",
    pr: { number: 41, draft: false, title: "fix: a bug", labels: ["bug", "p1"] },
    sender: { type: "User" },
    empty: "",
    when: "2026-08-21T10:00:00Z",
  },
};

function group(conditions: unknown[]): unknown {
  return { mode: "all", conditions };
}

describe("parseFilterGroup", () => {
  test("rejects unknown operators, unsafe paths, deep nesting, leaf overflow", () => {
    expect(() => parseFilterGroup(group([{ path: "a", op: "matchez" }]))).toThrow(ConditionParseError);
    expect(() => parseFilterGroup(group([{ path: "__proto__.x", op: "equals" }]))).toThrow(
      ConditionParseError,
    );
    const deep = { mode: "all", conditions: [{ mode: "all", conditions: [{ mode: "all", conditions: [{ mode: "all", conditions: [] }] }] }] };
    expect(() => parseFilterGroup(deep)).toThrow(/nesting/);
    const wide = group(
      Array.from({ length: CONDITION_MAX_LEAVES + 1 }, () => ({ path: "a", op: "is_empty" })),
    );
    expect(() => parseFilterGroup(wide)).toThrow(/conditions/);
  });

  test("rejects invalid and oversized regex patterns", () => {
    expect(() => parseFilterGroup(group([{ path: "a", op: "matches", value: "(" }]))).toThrow(
      ConditionParseError,
    );
    expect(() =>
      parseFilterGroup(group([{ path: "a", op: "matches", value: "x".repeat(300) }])),
    ).toThrow(ConditionParseError);
    expect(() => parseFilterGroup(group([{ path: "a", op: "in", value: "not-a-list" }]))).toThrow(
      ConditionParseError,
    );
  });

  test("rejects patterns that backtrack super-linearly", () => {
    for (const value of ["(a+)+$", "(.*)*x", "(\\w+\\s?)*$", "(a|aa)+$", "((a|aa))+$", "((foo|foobar)x?)+", "(?:(?:a|b)c)*", "a*a*a*a*b", "\\d+\\s+\\w+\\s+x{2,}", "(a)\\1", "(?<n>a)\\k<n>", "a{1,500}", "(a{2,}){2}"]) {
      expect(() => parseFilterGroup(group([{ path: "a", op: "matches", value }]))).toThrow(
        ConditionParseError,
      );
    }
    for (const value of ["^fix:", "a+b*", "(ab)+", "[a-z]+\\d{1,3}", "(?:x|y)?z", "(?<=a)b", "(a+)?", "a{3}", "\\d+\\.\\d+", "a*a*a*b"]) {
      expect(() => parseFilterGroup(group([{ path: "a", op: "matches", value }]))).not.toThrow();
    }
  });
});

describe("evaluateFilter", () => {
  const evaluate = (conditions: unknown[], mode: "all" | "any" = "all") =>
    evaluateFilter(parseFilterGroup({ mode, conditions }), scope);

  test("operator semantics", () => {
    expect(evaluate([{ path: "event.action", op: "equals", value: "opened" }])).toBe(true);
    expect(evaluate([{ path: "event.pr.number", op: "gt", value: 40 }])).toBe(true);
    expect(evaluate([{ path: "event.pr.number", op: "lt", value: 40 }])).toBe(false);
    expect(evaluate([{ path: "event.when", op: "gt", value: "2026-08-20" }])).toBe(true);
    expect(evaluate([{ path: "event.pr.title", op: "contains", value: "bug" }])).toBe(true);
    expect(evaluate([{ path: "event.pr.labels", op: "contains", value: "p1" }])).toBe(true);
    expect(evaluate([{ path: "event.pr.title", op: "matches", value: "^fix:" }])).toBe(true);
    expect(evaluate([{ path: "event.action", op: "in", value: ["opened", "reopened"] }])).toBe(true);
    expect(evaluate([{ path: "event.empty", op: "is_empty" }])).toBe(true);
    expect(evaluate([{ path: "event.pr.draft", op: "is_false" }])).toBe(true);
    expect(evaluate([{ path: "event.pr.draft", op: "is_true" }])).toBe(false);
  });

  test("gt/lt read numeric strings as numbers, not dates", () => {
    const s = { event: { n: "41", year: "2026", when: "2026-08-21T00:00:00Z" } };
    const run = (conditions: unknown[]) => evaluateFilter(parseFilterGroup({ mode: "all", conditions }), s);
    expect(run([{ path: "event.n", op: "gt", value: 40 }])).toBe(true);
    expect(run([{ path: "event.n", op: "lt", value: 40 }])).toBe(false);
    expect(run([{ path: "event.year", op: "gt", value: 100 }])).toBe(true);
    expect(run([{ path: "event.year", op: "gt", value: 3000 }])).toBe(false);
    expect(run([{ path: "event.year", op: "lt", value: "2027" }])).toBe(true);
    expect(run([{ path: "event.when", op: "gt", value: "2026-08-20" }])).toBe(true);
  });

  test("an unsafe pattern stored before the guard fails closed, fast", () => {
    // Bypass parse-time validation the way a pre-guard row would.
    const stored: FilterGroup = {
      mode: "all",
      conditions: [{ path: "event.text", op: "matches", value: "(a+)+$" }],
    };
    const startedAt = performance.now();
    expect(evaluateFilter(stored, { event: { text: `${"a".repeat(4096)}!` } })).toBe(false);
    expect(performance.now() - startedAt).toBeLessThan(200);
  });

  test("missing paths: false for everything except is_empty/not_equals", () => {
    expect(evaluate([{ path: "event.nope", op: "equals", value: "x" }])).toBe(false);
    expect(evaluate([{ path: "event.nope", op: "matches", value: "x" }])).toBe(false);
    expect(evaluate([{ path: "event.nope", op: "gt", value: 1 }])).toBe(false);
    expect(evaluate([{ path: "event.nope", op: "is_empty" }])).toBe(true);
    expect(evaluate([{ path: "event.nope", op: "not_equals", value: "x" }])).toBe(true);
  });

  test("all/any groups and nesting", () => {
    expect(
      evaluate(
        [
          { path: "event.action", op: "equals", value: "closed" },
          { path: "event.pr.draft", op: "is_false" },
        ],
        "any",
      ),
    ).toBe(true);
    const nested = parseFilterGroup({
      mode: "all",
      conditions: [
        { path: "event.sender.type", op: "not_equals", value: "Bot" },
        {
          mode: "any",
          conditions: [
            { path: "event.action", op: "equals", value: "closed" },
            { path: "event.action", op: "equals", value: "opened" },
          ],
        },
      ],
    });
    expect(evaluateFilter(nested, scope)).toBe(true);
  });
});
