import { Code, ConnectError } from "@connectrpc/connect";
import { eq } from "drizzle-orm";
import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { getDb } from "../db/client.ts";
import { task, taskSession, user } from "../db/schema.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { isServiceAccountEmail } from "../rpc/api-key.ts";
import type { GetSession } from "../rpc/require.ts";
import type { UnaryOverride } from "../rpc/passthrough.ts";
import {
  CompleteToolCallResponseSchema,
  GetMcpWritePolicyResponseSchema,
  type CompleteToolCallRequest,
  type GetMcpWritePolicyRequest,
} from "../gen/engram/app/v1/session_pb.ts";
import type {
  AcceptPlanExitRequestSchema,
  ReadWritePolicyRequestSchema,
  AcceptPlanExitResponseSchema,
  ReadWritePolicyResponseSchema,
} from "../gen/engram/mcp/v1/session_policy_pb.ts";

export interface SessionPolicyControl {
  acceptPlanExit?(
    request: MessageInitShape<typeof AcceptPlanExitRequestSchema>,
  ): Promise<MessageInitShape<typeof AcceptPlanExitResponseSchema>>;
  readWritePolicy?(
    request: MessageInitShape<typeof ReadWritePolicyRequestSchema>,
  ): Promise<MessageInitShape<typeof ReadWritePolicyResponseSchema>>;
}
/** Fresh principal and owner lookup. Cached session roles and owner projections do not authorize. */
export async function authorizePolicyActor(
  sessionId: string,
  actorId: string,
): Promise<{ id: string; isAdmin: boolean }> {
  const [row] = await getDb()
    .select({
      ownerId: task.createdByUserId,
      role: user.role,
      email: user.email,
      banned: user.banned,
    })
    .from(taskSession)
    .innerJoin(task, eq(taskSession.taskId, task.id))
    .innerJoin(user, eq(user.id, actorId))
    .where(eq(taskSession.sessionId, sessionId))
    .limit(1);
  if (
    !row ||
    row.banned ||
    isServiceAccountEmail(row.email) ||
    (row.role !== "admin" && row.ownerId !== actorId)
  )
    throw new ConnectError("human session owner or administrator required", Code.PermissionDenied);
  return { id: actorId, isAdmin: row.role === "admin" };
}
/** Overrides run after ordinary permission and tool-output guards, never as preflights. */
export function makeSessionPolicyOverrides(deps?: {
  control?: SessionPolicyControl;
  getSession?: GetSession;
  authorize?: typeof authorizePolicyActor;
}): Record<string, UnaryOverride> {
  const getSession = deps?.getSession ?? getSessionFromHeaders;
  const authorize = deps?.authorize ?? authorizePolicyActor;
  return {
    async "SessionService.CompleteToolCall"(request, ctx) {
      const req = request as CompleteToolCallRequest;
      if (!req.expectedMcpPlanReview) return undefined;
      let result: unknown;
      try {
        result = JSON.parse(req.resultJson);
      } catch {
        throw new ConnectError("invalid plan result", Code.InvalidArgument);
      }
      if (
        !result ||
        typeof result !== "object" ||
        !("decision" in result) ||
        result.decision !== "approve"
      ) {
        // Generic rejection uses the existing delivery path and cannot change policy.
        req.expectedMcpPlanReview = undefined;
        return undefined;
      }
      const review = req.expectedMcpPlanReview;
      if (review.policyRef?.sessionId !== req.sessionId || review.toolCallId !== req.toolCallId)
        throw new ConnectError("plan review changed; reload the plan", Code.FailedPrecondition);
      const session = await getSession(ctx.requestHeader);
      if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
      const actor = await authorize(req.sessionId, session.user.id);
      const accept = deps?.control?.acceptPlanExit;
      if (!accept)
        throw new ConnectError("session policy control unavailable", Code.FailedPrecondition);
      try {
        await accept({
          protocol: 1,
          expected: review,
          actorId: actor.id,
          resultJson: req.resultJson,
        });
      } catch {
        throw new ConnectError(
          "session policy acceptance unavailable; reload the plan",
          Code.FailedPrecondition,
        );
      }
      return {
        response: create(CompleteToolCallResponseSchema, {
          sessionId: req.sessionId,
          note: "tool result queued",
        }),
      };
    },
    async "SessionService.GetMcpWritePolicy"(request, ctx) {
      const req = request as GetMcpWritePolicyRequest;
      const session = await getSession(ctx.requestHeader);
      if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
      await authorize(req.sessionId, session.user.id);
      const read = deps?.control?.readWritePolicy;
      if (!read)
        throw new ConnectError("session policy control unavailable", Code.FailedPrecondition);
      try {
        const result = await read({
          protocol: 1,
          sessionId: req.sessionId,
          planToolCallId: req.planToolCallId,
        });
        if (!result.state) throw new Error("policy state missing");
        const response = create(GetMcpWritePolicyResponseSchema, result.state);
        response.allowedActions = response.planReview
          ? ["approve_current_plan"]
          : !response.policy?.initialized
            ? ["start_supported_session"]
            : response.policy.writeEligibility !== "write_eligible" &&
                response.policy.ancestorPolicyRefs.length
              ? ["inspect_ancestor_policy"]
              : [];
        return { response };
      } catch {
        throw new ConnectError("session policy read unavailable", Code.FailedPrecondition);
      }
    },
  };
}

export interface RequestPolicyContext {
  sessionId: string;
  runId: string;
  toolCallId: string;
  acceptedPolicyRef: import("../gen/engram/app/v1/session_pb.ts").McpWritePolicyRef;
  acceptedWriteEligibility: string;
  ancestorPolicyRefs: import("../gen/engram/app/v1/session_pb.ts").McpWritePolicyRef[];
}
/** The journal persists the returned context before any authorization or dispatch. */
export function makeSessionWritePolicy(
  control: import("./control-client.ts").SessionPolicyControlClient,
) {
  return {
    async readForRequest(input: { sessionId: string }) {
      const response = await control.readWritePolicy({ protocol: 1, sessionId: input.sessionId });
      if (!response.state?.policy?.ref || response.state.policy.ref.sessionId !== input.sessionId)
        throw new ConnectError("session policy unavailable", Code.FailedPrecondition);
      return response.state.policy;
    },
    async validateExecutionIdentity(
      context: RequestPolicyContext,
      expected: { modelToolName: string; originalArguments: string },
    ) {
      const result = await control.validateExecutionIdentity({
        protocol: 1,
        ...context,
        ...expected,
      });
      if (result.protocol !== 1 || !["current", "stale", "unknown"].includes(result.status))
        throw new ConnectError("session identity fence unavailable", Code.FailedPrecondition);
      return result.status;
    },
    /** After the durable barrier, immediately before dispatch. Never cache a success. */
    async validateDispatchFence(
      context: RequestPolicyContext,
      expected: { modelToolName: string; originalArguments: string },
    ) {
      const result = await control.validateDispatchFence({ protocol: 1, ...context, ...expected });
      if (result.protocol !== 1 || !["current", "stale", "unknown"].includes(result.status))
        throw new ConnectError("session policy fence unavailable", Code.FailedPrecondition);
      return result.status;
    },
  };
}
