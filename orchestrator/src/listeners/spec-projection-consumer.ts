import type { CuratedEvent } from "../control-plane/session-events.ts";
import { log as rootLog } from "../log.ts";
import {
  SPEC_PROJECTION_PATH,
  type SpecProjectionDriver,
} from "../specs/projection.ts";
import type { SessionConsumer } from "./consumer.ts";

const log = rootLog.child({ component: "spec-projection-consumer" });

function changedProjectionPath(event: CuratedEvent): boolean {
  if (event.kind !== "file_changed") return false;
  try {
    const payload = JSON.parse(event.payloadJson) as { path?: unknown };
    return payload.path === SPEC_PROJECTION_PATH;
  } catch {
    return false;
  }
}

export function makeSpecProjectionConsumer(projection: SpecProjectionDriver): SessionConsumer {
  return {
    name: "spec-projection",
    appliesTo: (sessionId) => projection.appliesTo(sessionId),
    interestedIn: (kind) =>
      kind === "run_completed" ||
      kind === "harness_idle" ||
      kind === "harness_parked" ||
      kind === "resumed" ||
      kind === "tool_call_completed" ||
      kind === "file_changed",
    async handle(event, ctx) {
      const specId = await projection.storeSpecForSession(ctx.sessionId);
      if (!specId) return;
      if (event.kind === "run_completed" || changedProjectionPath(event)) {
        await projection.checkDrift(specId, ctx.sessionId);
      }
      if (
        event.kind === "run_completed" ||
        event.kind === "harness_idle" ||
        event.kind === "harness_parked" ||
        event.kind === "resumed"
      ) {
        await projection.enqueue({
          specId,
          sessionId: ctx.sessionId,
          source: event.kind,
        });
      }
      try {
        await projection.runOnce(ctx.sessionId);
      } catch (error) {
        log.error(
          { sessionId: ctx.sessionId, specId, error },
          "spec projection publish failed",
        );
        throw error;
      }
    },
  };
}
