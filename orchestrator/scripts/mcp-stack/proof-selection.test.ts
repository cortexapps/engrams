import { expect, test } from "bun:test";
import { parseGuestProofRecords } from "./guest.ts";
import { selectProofRecords } from "./scenario-runtime.ts";

const plan = `sha256:${"a".repeat(64)}`;
const tool = "mcp_fixture_read";
const initial = {
  callId: "resume-read-1",
  tool,
  planSha256: plan,
  resultReceived: true,
};
const denied = {
  callId: "resume-read-1",
  tool,
  planSha256: plan,
  expectExposure: "absent",
  selectedToolName: tool,
  requestAttempted: true,
  protocolDenialCode: "unknown_tool",
  exposedMcpToolNamesSha256: `sha256:${"b".repeat(64)}`,
} as const;
const selection = {
  callId: "resume-read-1",
  tool,
  planSha256: plan,
  expectExposure: "absent" as const,
};
function records(rows: object[]) {
  return parseGuestProofRecords(
    Buffer.from(rows.map((row) => JSON.stringify(row) + "\n").join("")),
  );
}

test("selects the revoked proof whether the old proof file survives or is empty", () => {
  for (const old of [[], [initial]]) {
    const found = selectProofRecords(
      [...records(old), ...records([denied])],
      selection,
    );
    expect(found).toHaveLength(1);
    expect(found[0]!.proof).toEqual(denied);
  }
});

test("rejects positive or mismatched proofs for the revoked call", () => {
  for (const row of [
    initial,
    { ...denied, callId: "other" },
    { ...denied, tool: "mcp_other", selectedToolName: "mcp_other" },
    { ...denied, planSha256: `sha256:${"c".repeat(64)}` },
  ]) {
    expect(selectProofRecords(records([row]), selection)).toHaveLength(0);
  }
});
