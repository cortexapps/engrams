import { Code, ConnectError } from "@connectrpc/connect";
import type { MessageInitShape } from "@bufbuild/protobuf";
import {
  type Profile,
  type ProfileIntegrationGrant,
  type UpdateProfileRequestSchema,
} from "../../src/gen/engram/app/v1/profile_pb.ts";
import {
  McpRebindDrainReason,
  McpRebindDrainSchema,
} from "../../src/gen/engram/app/v1/integration_pb.ts";
import { bindingTools, expectRejected, waitFor } from "./bootstrap.ts";
import { requireThat, RunnerFailure, type Context } from "./context.ts";
import { canonicalJson, guestReady } from "./guest.ts";
import type { DatabaseObserver } from "./observer.ts";
import type { StartedScenario } from "./scenario-runtime.ts";
import type { PublicClient } from "./transport.ts";

/** UpdateProfile replaces the profile body; retain every unrelated public field. */
export function grantUpdateRequest(
  profile: Profile,
  integrationGrants: ProfileIntegrationGrant[],
): MessageInitShape<typeof UpdateProfileRequestSchema> {
  return {
    id: profile.id,
    name: profile.name,
    description: profile.description,
    icon: profile.icon,
    imageId: profile.imageId,
    includeUserTokens: profile.includeUserTokens,
    envVars: profile.envVars,
    skills: profile.skills,
    network: profile.network,
    secrets: profile.secrets,
    harness: profile.harness,
    model: profile.model,
    effort: profile.effort,
    integrationGrants,
    repos: profile.repos,
    modelRouter: profile.modelRouter,
    apps: profile.apps,
    programmaticOnly: profile.programmaticOnly,
    expectedMcpGrantRevision: profile.mcpGrantRevision,
  };
}
export async function revokeProfileGrants(
  admin: PublicClient,
  profileId: string,
) {
  const current = await admin.profiles.getProfile({ id: profileId });
  requireThat(current.profile?.id === profileId, "revoke_actual_profile");
  const updated = await admin.profiles.updateProfile(
    grantUpdateRequest(current.profile, []),
  );
  requireThat(
    updated.profile?.id === profileId &&
      updated.profile.integrationGrants.length === 0 &&
      updated.profile.mcpGrantRevision ===
        current.profile.mcpGrantRevision + 1n,
    "empty_grants_publication",
  );
  return updated.profile;
}

type RebindRequest = {
  sessionId: string;
  expectedBindingHash: string;
  expectedCandidateHash: string;
};

type OAuthRebindChange = "account" | "scope" | "reconnect";
type RecoveryStage = "prepare" | "commit" | "started";
class RebindPreconditionFailure extends RunnerFailure {}
const recoverySeams = new Set([
  "reviewed_rebind_candidate",
  "managed_rebind_commit_started",
  "rebind_final_binding_hash",
  "managed_cold_recovery_deadline",
  "managed_cold_recovery_started",
  "managed_cold_recovery_identity",
  "managed_cold_recovery_orchestrator_started",
  "managed_cold_recovery_reviewed_binding",
  "managed_cold_recovery_active",
]);

/** Only local fixed markers and validated phases enter failure evidence. */
export async function withRebindDiagnostics<T>(
  change: OAuthRebindChange | undefined,
  stage: RecoveryStage,
  readPhase: () => Promise<{ phase: string }>,
  run: () => Promise<T>,
): Promise<T> {
  if (!change) return run();
  try {
    return await run();
  } catch (error) {
    let phase = "unavailable";
    try {
      const observed = await readPhase();
      if (
        [
          "reserving",
          "held",
          "preparing",
          "finalizing",
          "started",
          "recovering",
          "terminal",
        ].includes(observed.phase)
      )
        phase = observed.phase;
    } catch {
      // An unavailable observation cannot replace the original failed boundary.
    }
    const reason =
      error instanceof RebindPreconditionFailure ||
      (error instanceof RunnerFailure && recoverySeams.has(error.seam))
        ? error.seam
        : "unclassified";
    throw new RunnerFailure(
      "ASSERTION_FAILED",
      `oauth_${change}_recovery_${stage}_phase_${phase}_${reason}`,
    );
  }
}

/** Replay the same reviewed request only while the server reports a typed drain. */
export async function commitReviewedRebind(
  ctx: Pick<Context, "deadlines">,
  request: RebindRequest,
  commit: (request: RebindRequest) => Promise<{ bindingHash: string }>,
) {
  const result = await waitFor(
    ctx,
    "managed_rebind_commit_started",
    async () => {
      try {
        return await commit(request);
      } catch (error) {
        if (
          !(error instanceof ConnectError) ||
          error.code !== Code.FailedPrecondition
        )
          throw error;
        const details = error.findDetails(McpRebindDrainSchema);
        const validRetry =
          details.length === 1 &&
          Number.isFinite(Date.parse(details[0]!.retryAfter));
        if (
          details.length !== 1 ||
          details[0]!.reason !== McpRebindDrainReason.DISPATCH_IN_PROGRESS ||
          !validRetry
        ) {
          const count =
            details.length === 0 ? "0" : details.length === 1 ? "1" : "many";
          const reason =
            details.length !== 1
              ? "none"
              : details[0]!.reason === McpRebindDrainReason.DISPATCH_IN_PROGRESS
                ? "dispatch"
                : details[0]!.reason === McpRebindDrainReason.UNKNOWN_INTENT
                  ? "unknown_intent"
                  : details[0]!.reason === McpRebindDrainReason.LEGACY_INTENT
                    ? "legacy_intent"
                    : details[0]!.reason === McpRebindDrainReason.UNSPECIFIED
                      ? "unspecified"
                      : "other";
          const retry =
            details.length === 1 ? (validRetry ? "valid" : "invalid") : "na";
          const category =
            new Map([
              ["MCP calls are still draining", "draining"],
              [
                "An administrator must publish MCP grants before rebind",
                "publication",
              ],
              ["MCP operation could not be completed", "operation"],
              [
                "external MCP service is not configured",
                "service_unconfigured",
              ],
              [
                "managed boot authority changed or unavailable",
                "boot_authority",
              ],
              ["session policy changed or unavailable", "policy_changed"],
              ["host policy unavailable", "host_policy"],
              ["retained policy unavailable", "retained_policy"],
              ["managed guest identity unavailable", "guest_identity"],
              ["managed egress source unavailable", "egress_source"],
              ["managed policy does not support tunnels", "tunnels"],
              ["desired policy changed", "desired_policy"],
              ["managed guest identity changed", "guest_identity_changed"],
              ["host acknowledgement mismatch", "host_ack"],
              ["physical placement changed", "placement"],
              ["managed creation authority changed", "creation_authority"],
            ]).get(error.rawMessage) ?? "other";
          throw new RebindPreconditionFailure(
            "ASSERTION_FAILED",
            `rebind_precondition_details_${count}_reason_${reason}_retry_${retry}_category_${category}`,
          );
        }
        return null;
      }
    },
    (value) => value !== null,
  );
  requireThat(
    result &&
      /^sha256:[a-f0-9]{64}$/.test(result.bindingHash) &&
      result.bindingHash !== request.expectedBindingHash,
    "rebind_final_binding_hash",
  );
  return result;
}

/** The supported managed resume is reviewed cold recovery; ordinary Resume stays fenced. */
export async function recoverCurrentBinding(
  ctx: Context,
  scenario: StartedScenario,
  database: DatabaseObserver,
  change?: OAuthRebindChange,
) {
  const old = await database.started(scenario.sessionId, ctx.hostId);
  requireThat(
    old.phase === "started" && old.binding_hash,
    "rebind_started_source",
  );
  const oldHash = old.binding_hash;
  const ordinaryResumeDenied = await expectRejected(
    () =>
      scenario.reviewClient.sessions.resume({ sessionId: scenario.sessionId }),
    [Code.Unavailable],
  );
  const observe = <T>(stage: RecoveryStage, run: () => Promise<T>) =>
    withRebindDiagnostics(
      change,
      stage,
      () => database.managedLifecycle(scenario.sessionId, old),
      run,
    );
  const candidate = await observe("prepare", async () => {
    const prepared = await scenario.reviewClient.integration.prepareMcpRebind({
      sessionId: scenario.sessionId,
      expectedBindingHash: oldHash,
    });
    requireThat(
      /^sha256:[a-f0-9]{64}$/.test(prepared.bindingHash) &&
        prepared.bindingHash !== oldHash &&
        prepared.bindingEpoch === String(Number(old.binding_epoch) + 1) &&
        prepared.drain === undefined,
      "reviewed_rebind_candidate",
    );
    return prepared;
  });
  const deadline = performance.now() + ctx.deadlines.stepMs;
  const committed = await observe("commit", () =>
    commitReviewedRebind(
      ctx,
      {
        sessionId: scenario.sessionId,
        expectedBindingHash: oldHash,
        expectedCandidateHash: candidate.bindingHash,
      },
      (request) => scenario.reviewClient.integration.rebindMcp(request),
    ),
  );
  return observe("started", async () => {
    const remainingMs = deadline - performance.now();
    requireThat(remainingMs > 0, "managed_cold_recovery_deadline");
    const current = await waitFor(
      { deadlines: { ...ctx.deadlines, stepMs: remainingMs } },
      "managed_cold_recovery_started",
      () => database.started(scenario.sessionId, ctx.hostId),
      (value) =>
        value.phase === "started" &&
        value.binding_hash === committed.bindingHash,
    );
    requireThat(
      current.binding_epoch === candidate.bindingEpoch &&
        current.source_key.context.target.sandbox_id !==
          old.source_key.context.target.sandbox_id &&
        current.boot_attempt !== old.boot_attempt &&
        current.native_digest === old.native_digest &&
        canonicalJson(current.source_key.creation) ===
          canonicalJson(old.source_key.creation),
      "managed_cold_recovery_identity",
    );
    await waitFor(
      ctx,
      "managed_cold_recovery_orchestrator_started",
      () => database.managedLifecycle(scenario.sessionId, current),
      (value) => value.phase === "started",
    );
    const bound = await bindingTools(scenario.reviewClient, scenario.sessionId);
    requireThat(
      canonicalJson(bound) === canonicalJson(candidate.bindingTools),
      "managed_cold_recovery_reviewed_binding",
    );
    const session = await scenario.reviewClient.sessions.getSession({
      sessionId: scenario.sessionId,
    });
    requireThat(
      session.session?.status === "active",
      "managed_cold_recovery_active",
    );
    await guestReady(ctx, scenario.client, scenario.sessionId);
    return {
      old,
      current,
      candidate,
      finalBindingHash: committed.bindingHash,
      ordinaryResumeDenied,
    };
  });
}
