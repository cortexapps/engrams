import { describe, expect, it } from "vitest";

import { mergePaths, schemaPaths } from "./event-paths";

describe("schemaPaths", () => {
  it("lists every declared field under event.raw, parents included, arrays stopped", () => {
    const schema = JSON.stringify({
      type: "object",
      properties: {
        action: { type: "string" },
        issue: {
          type: "object",
          properties: {
            title: { type: "string" },
            labels: { type: "array", items: { type: "object", properties: { name: {} } } },
          },
        },
      },
    });
    expect(schemaPaths(schema)).toEqual([
      "event.raw.action",
      "event.raw.issue",
      "event.raw.issue.title",
      "event.raw.issue.labels",
    ]);
  });

  it("is empty for no schema or a schema that does not parse", () => {
    expect(schemaPaths("")).toEqual([]);
    expect(schemaPaths("{nope")).toEqual([]);
  });
});

describe("mergePaths", () => {
  it("keeps the static order and appends previewed paths once", () => {
    expect(
      mergePaths(["trigger.kind", "event.raw"], {
        "event.raw": "{…}",
        "event.raw.action": "opened",
        "trigger.kind": "integration",
      }),
    ).toEqual(["trigger.kind", "event.raw", "event.raw.action"]);
  });
});
