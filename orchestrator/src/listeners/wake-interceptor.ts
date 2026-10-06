/** The listener wake (ADR 0119 amendment, 2026-10-05): a dormant listener
 * row is a parked session, and a parked session resumes only through an
 * orchestrator RPC on the coordinator. This interceptor sits on the
 * control-plane transport, so every path that can resume a session — the
 * passthrough surface, the engine's session blocks, the coordination tools —
 * wakes the row after the RPC returns, and the scanner re-arms the listener
 * on its next pass. The wake is a no-op for a row that is not dormant. */

import type { Interceptor } from "@connectrpc/connect";
import type { Logger } from "pino";

import { SessionService } from "../gen/engram/app/v1/session_pb.ts";

/** Unary SessionService methods that can resume or end a parked session.
 * Reads (GetSession, ListSessions, ListSessionEvents, GetLog, …) are not
 * here: they are frequent and change nothing. The streaming methods (Exec,
 * WriteFile) carry their request in a stream the interceptor does not read;
 * the manager's dormant reconcile covers them. */
export const WAKE_METHODS: ReadonlySet<string> = new Set([
  "SendPrompt",
  "Interrupt",
  "Resume",
  "CancelExec",
  "CompleteToolCall",
  "EditQueuedPrompt",
  "DequeueQueuedPrompt",
  "Snapshot",
  "EnsureBrowser",
  "EnsureIde",
  "EvictLocal",
  "EvictIdle",
  "DeleteSession",
  "CreateArtifactFromPath",
]);

export interface WakeRequestShape {
  stream: boolean;
  service: { typeName: string };
  method: { name: string };
  message: unknown;
}

/** The session a request wakes, or undefined when it wakes none. */
export function wakeTarget(req: WakeRequestShape): string | undefined {
  if (req.stream) return undefined;
  if (req.service.typeName !== SessionService.typeName) return undefined;
  if (!WAKE_METHODS.has(req.method.name)) return undefined;
  const sessionId = (req.message as { sessionId?: unknown } | null)?.sessionId;
  return typeof sessionId === "string" && sessionId !== "" ? sessionId : undefined;
}

export function makeListenerWakeInterceptor(
  wake: (sessionId: string) => Promise<void>,
  log: Logger,
): Interceptor {
  return (next) => async (req) => {
    const sessionId = wakeTarget(req);
    try {
      return await next(req);
    } finally {
      // Issued after the RPC settles, whatever its outcome (a failed resume
      // wakes a row the listener then reconciles) — and never awaited: the
      // wake is best-effort, so the caller's response must not wait on the
      // orchestrator's database. A lost or failed wake is healed by the
      // dormant reconcile within its interval.
      if (sessionId !== undefined) {
        void wake(sessionId).catch((err: unknown) => {
          log.warn({ sessionId, err }, "listener wake failed after a session rpc");
        });
      }
    }
  };
}
