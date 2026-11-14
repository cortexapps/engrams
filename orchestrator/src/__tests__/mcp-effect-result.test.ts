import { expect, test } from "bun:test";
import { verifiedExecutionResult } from "../mcp/effect-driver.ts";

test("verified result keeps upstream outcome and delivery separate", () => {
  expect(
    verifiedExecutionResult({
      outcome: "succeeded",
      delivery: "unavailable",
      reason: "output_schema",
    }),
  ).toEqual({
    state: "succeeded",
    result: { outcome: "succeeded", delivery: "unavailable", reason: "output_schema" },
    resultUnavailable: true,
  });
  const error = {
    outcome: "failed",
    delivery: "available",
    output: {
      content: [{ type: "text", text: "rejected" }],
      structuredContent: { code: 1 },
      isError: true,
    },
  };
  expect(verifiedExecutionResult(error)).toEqual({
    state: "failed",
    result: error,
    resultUnavailable: false,
  });
  expect(
    verifiedExecutionResult({
      outcome: "outcome_unknown",
      delivery: "unavailable",
      reason: "upstream_failure",
    }).state,
  ).toBe("outcome_unknown");
});

test("malformed and contradictory outcomes never establish success", () => {
  for (const value of [
    null,
    {},
    { outcome: "succeeded" },
    {
      outcome: "succeeded",
      delivery: "available",
      output: { content: [{ type: "image", data: "AA==" }] },
    },
    { outcome: "succeeded", delivery: "available", output: { content: [], isError: true } },
    { outcome: "outcome_unknown", delivery: "available", output: { content: [] } },
    { outcome: "failed", delivery: "unavailable", reason: "invented" },
    { outcome: "succeeded", delivery: "unavailable", reason: "output_schema", secret: "untrusted" },
  ]) {
    expect(() => verifiedExecutionResult(value)).toThrow();
  }
});
