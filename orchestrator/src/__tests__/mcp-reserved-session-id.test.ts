import { expect, test } from "bun:test";
import {
  createReservedTaskSession,
  McpSessionCreationMismatchError,
} from "../mcp/session-create.ts";
test("reserved creation retains the actual response ID for child compensation", async () => {
  let error: unknown;
  try {
    await createReservedTaskSession(
      { sessions: { createSession: async () => ({ sessionId: "actual-session" }) } },
      { imageUri: "image", mode: "agent", requestedSessionId: "reserved-session" },
      { actorId: "actor", inputId: "input" },
    );
  } catch (caught) {
    error = caught;
  }
  expect(error).toBeInstanceOf(McpSessionCreationMismatchError);
  if (!(error instanceof McpSessionCreationMismatchError)) throw Error("missing identity");
  expect(error.createdSessionId).toBe("actual-session");
});
