import { describe, expect, it } from "vitest";

import { flattenScope, variableValuesFrom, type TestRenderResult } from "./useAutomationTest";

describe("flattenScope", () => {
  it("flattens nested scope into path → preview, previewing nodes and leaves", () => {
    const values = flattenScope({
      inputs: { mention: "@engrams" },
      event: { raw: { issue: { title: "boom", number: 41 } }, labels: ["bug", "p1"] },
    });
    expect(values["inputs.mention"]).toBe("@engrams");
    expect(values["event.raw.issue.title"]).toBe("boom");
    expect(values["event.raw.issue.number"]).toBe("41");
    // Arrays preview whole; objects preview as JSON at every level.
    expect(values["event.labels"]).toBe('["bug","p1"]');
    expect(values["event.raw.issue"]).toBe('{"title":"boom","number":41}');
  });

  it("truncates long previews and never walks inherited keys", () => {
    const long = "x".repeat(200);
    const values = flattenScope({ steps: { find: { stdout: long } } });
    expect(values["steps.find.stdout"]!.length).toBeLessThan(90);
    expect(values["steps.find.stdout"]!.endsWith("…")).toBe(true);
    expect(Object.keys(values).some((k) => k.includes("__proto__"))).toBe(false);
  });
});

describe("variableValuesFrom", () => {
  it("merges every block's scope so later outputs appear for the picker", () => {
    const result: TestRenderResult = {
      blocks: [
        {
          blockId: "check",
          blockType: "filter",
          rendered: {},
          filterPass: true,
          scope: { inputs: { mention: "@engrams" }, steps: {} },
        },
        {
          blockId: "launch",
          blockType: "create_session",
          rendered: {},
          scope: {
            inputs: { mention: "@engrams" },
            steps: { check: { pass: true }, launch: { session_id: "s-1" } },
          },
        },
      ],
      errors: [],
    };
    const values = variableValuesFrom(result);
    expect(values["inputs.mention"]).toBe("@engrams");
    expect(values["steps.launch.session_id"]).toBe("s-1");
    expect(variableValuesFrom(null)).toEqual({});
  });
});
