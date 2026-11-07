import { expect, test } from "bun:test";
import { create } from "@bufbuild/protobuf";
import { ListSessionEventsResponseSchema } from "../../src/gen/engram/app/v1/session_pb.ts";
import { toolCompletionEvents } from "./scenario-runtime.ts";
import { selectColdDenial } from "./case-physical.ts";
import {
  assertDuplicateWriteUnknown,
  assertNewIdWriteDenial,
} from "./case-recovery.ts";
import { canonicalJson, parseGuestProofRecords } from "./guest.ts";
import { sha256 } from "./context.ts";

const alias = "mcp_fixture_read";
const callId = "cold-old-read";
const planSha256 = `sha256:${"a".repeat(64)}`;
const emptyExposure = `sha256:${sha256(canonicalJson([]))}`;
const records = (proof: object) =>
  parseGuestProofRecords(Buffer.from(JSON.stringify(proof) + "\n"));
const positive = () =>
  records({
    callId,
    tool: alias,
    planSha256,
    resultReceived: true,
    delivery: "available",
    outcome: "succeeded",
    isError: false,
  });
const negative = () =>
  records({
    callId,
    tool: alias,
    planSha256,
    expectExposure: "absent",
    selectedToolName: alias,
    requestAttempted: true,
    protocolDenialCode: "unknown_tool",
    exposedMcpToolNamesSha256: emptyExposure,
  });

for (const harness of ["claude", "codex"] as const) {
  const nativeName = harness === "claude" ? `mcp__engrams__${alias}` : alias;
  const payload = (id: string, name = nativeName) =>
    JSON.stringify({
      tool_call_id: id,
      tool_name: name,
      run_id: "actual-run",
      ok: true,
    });
  test(`${harness} completion uses exact native query and preserves call/run identity`, async () => {
    const selected = await toolCompletionEvents({
      alias,
      sessionId: "session",
      input: { harness, callId },
      reviewClient: {
        sessions: {
          listSessionEvents: async (request) => {
            expect(request).toEqual({
              sessionId: "session",
              kinds: ["tool_call_completed"],
              toolNames: [nativeName],
              limit: 129n,
            });
            return create(ListSessionEventsResponseSchema, {
              events: [
                {
                  idx: 7n,
                  kind: "tool_call_completed",
                  payloadJson: payload(callId),
                },
                {
                  idx: 8n,
                  kind: "tool_call_completed",
                  payloadJson: payload("other-call"),
                },
              ],
            });
          },
        },
      },
    });
    expect(selected).toEqual([
      {
        tool_call_id: callId,
        tool_name: nativeName,
        run_id: "actual-run",
        ok: true,
        idx: "7",
        sha256: sha256(payload(callId)),
      },
    ]);
  });
  test(`${harness} completion rejects a non-native name even if returned by server`, async () => {
    await expect(
      toolCompletionEvents({
        alias,
        sessionId: "session",
        input: { harness, callId },
        reviewClient: {
          sessions: {
            listSessionEvents: async () =>
              create(ListSessionEventsResponseSchema, {
                events: [
                  {
                    idx: 7n,
                    kind: "tool_call_completed",
                    payloadJson: payload(
                      callId,
                      harness === "claude" ? alias : `mcp__engrams__${alias}`,
                    ),
                  },
                ],
              }),
          },
        },
      }),
    ).rejects.toThrow("completion_event_shape");
  });
  test(`${harness} target-gone absence detects an actual native completion`, async () => {
    const scenario = {
      alias,
      sessionId: "session",
      input: { harness, callId },
      reviewClient: {
        sessions: {
          listSessionEvents: async (
            request: Parameters<
              Parameters<
                typeof toolCompletionEvents
              >[0]["reviewClient"]["sessions"]["listSessionEvents"]
            >[0],
          ) =>
            create(ListSessionEventsResponseSchema, {
              events: request.toolNames?.includes(nativeName)
                ? [
                    {
                      idx: 7n,
                      kind: "tool_call_completed",
                      payloadJson: payload(callId),
                    },
                  ]
                : [],
            }),
        },
      },
    };
    expect((await toolCompletionEvents(scenario)).length === 0).toBe(false);
    expect((await toolCompletionEvents(scenario, "other-call")).length).toBe(0);
  });
}

for (const order of ["old-first", "new-first"] as const) {
  test(`cold refusal selects new record with retained positive JSONL in ${order} filename order`, () => {
    const baseline = positive();
    const refused = negative();
    const all =
      order === "old-first"
        ? [...baseline, ...refused]
        : [...refused, ...baseline];
    expect(selectColdDenial(baseline, all)).toEqual(refused);
    expect(baseline).toEqual(positive());
  });
}
test("cold refusal rejects lost, replaced, duplicate or extra baseline/proof records", () => {
  const baseline = positive(),
    refused = negative();
  const replaced = records({ ...baseline[0]!.proof, isError: true });
  for (const all of [
    refused,
    [...replaced, ...refused],
    [...baseline, ...refused, ...refused],
    baseline,
  ]) {
    expect(() => selectColdDenial(baseline, all)).toThrow();
  }
  expect(() => selectColdDenial([], refused)).toThrow();
});
test("cold refusal requires exact same call, tool, plan and empty actual exposure", () => {
  const baseline = positive(),
    refused = negative();
  for (const change of [
    { callId: "other-call" },
    { tool: "mcp_other", selectedToolName: "mcp_other" },
    { planSha256: `sha256:${"b".repeat(64)}` },
    { exposedMcpToolNamesSha256: `sha256:${"b".repeat(64)}` },
  ]) {
    expect(() =>
      selectColdDenial(baseline, [
        ...baseline,
        ...records({ ...refused[0]!.proof, ...change }),
      ]),
    ).toThrow();
  }
});
test("new-ID unknown-write restriction requires actual available runtime refusal", () => {
  expect(() =>
    assertNewIdWriteDenial(
      records({
        callId: "held-write-2",
        tool: alias,
        planSha256,
        resultReceived: true,
        delivery: "available",
        outcome: null,
        isError: true,
      }),
    ),
  ).not.toThrow();
});
test("new-ID unknown-write restriction rejects unavailable or missing runtime refusal", () => {
  for (const change of [
    { delivery: "unavailable", outcome: "outcome_unknown" },
    { delivery: "unavailable", outcome: "failed" },
    { outcome: "failed" },
    { outcome: "succeeded", isError: false },
    { isError: false },
    { resultReceived: false },
  ])
    expect(() =>
      assertNewIdWriteDenial(
        records({
          callId: "held-write-2",
          tool: alias,
          planSha256,
          resultReceived: true,
          delivery: "available",
          outcome: null,
          isError: true,
          ...change,
        }),
      ),
    ).toThrow();
  expect(() => assertNewIdWriteDenial([])).toThrow();
});

test("duplicate unknown-write proof rejects invented success in either file order", () => {
  const unknown = records({
    callId: "held-write-1",
    tool: alias,
    planSha256,
    resultReceived: true,
    delivery: "unavailable",
    outcome: "outcome_unknown",
  });
  const success = records({
    ...unknown[0]!.proof,
    delivery: "available",
    outcome: "succeeded",
    isError: false,
  });
  for (const unordered of [
    [...success, ...unknown],
    [...unknown, ...success],
  ])
    expect(() => assertDuplicateWriteUnknown(unordered)).toThrow();
  expect(() =>
    assertDuplicateWriteUnknown([...unknown, ...unknown]),
  ).not.toThrow();
  expect(() => assertDuplicateWriteUnknown([])).toThrow();
});
