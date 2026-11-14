import type { Interceptor } from "@connectrpc/connect";
import { SessionService } from "../gen/engram/app/v1/session_pb.ts";
import type { McpInternalCancellationCause } from "./journal.ts";

const causes: Readonly<Record<string, McpInternalCancellationCause>> = {
  Interrupt: "interrupt",
  DeleteSession: "end",
  Resume: "resume",
  EvictLocal: "kill",
  EvictIdle: "kill",
};

/** Trusted session transport: commit cancellation before the coordinator changes lifecycle. */
export function makeMcpLifecycleInterceptor(
  invalidate: (id: string, cause: McpInternalCancellationCause) => Promise<void>,
): Interceptor {
  return (next) => async (req) => {
    if (!req.stream && req.service.typeName === SessionService.typeName) {
      const cause = causes[req.method.name];
      const id = (req.message as { sessionId?: unknown } | null)?.sessionId;
      if (cause && typeof id === "string" && id) await invalidate(id, cause);
    }
    return next(req);
  };
}
