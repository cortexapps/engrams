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
import { requireThat, type Context } from "./context.ts";
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

/** The supported managed resume is reviewed cold recovery; ordinary Resume stays fenced. */
export async function recoverCurrentBinding(
  ctx: Context,
  scenario: StartedScenario,
  database: DatabaseObserver,
) {
  const old = await database.started(scenario.sessionId, ctx.hostId);
  requireThat(
    old.phase === "started" && old.binding_hash,
    "rebind_started_source",
  );
  const ordinaryResumeDenied = await expectRejected(
    () =>
      scenario.reviewClient.sessions.resume({ sessionId: scenario.sessionId }),
    [Code.Unavailable],
  );
  const candidate = await scenario.reviewClient.integration.prepareMcpRebind({
    sessionId: scenario.sessionId,
    expectedBindingHash: old.binding_hash,
  });
  requireThat(
    /^sha256:[a-f0-9]{64}$/.test(candidate.bindingHash) &&
      candidate.bindingHash !== old.binding_hash &&
      candidate.bindingEpoch === String(Number(old.binding_epoch) + 1) &&
      candidate.drain === undefined,
    "reviewed_rebind_candidate",
  );
  try {
    const committed = await scenario.reviewClient.integration.rebindMcp({
      sessionId: scenario.sessionId,
      expectedBindingHash: old.binding_hash,
      expectedCandidateHash: candidate.bindingHash,
    });
    requireThat(
      committed.bindingHash === candidate.bindingHash,
      "rebind_committed_candidate",
    );
  } catch (error) {
    if (
      !(error instanceof ConnectError) ||
      error.code !== Code.FailedPrecondition
    )
      throw error;
    const details = error.findDetails(McpRebindDrainSchema);
    // Only the typed asynchronous recovery response may continue to actual observations.
    requireThat(
      details.length === 1 &&
        details[0]!.reason === McpRebindDrainReason.DISPATCH_IN_PROGRESS &&
        Number.isFinite(Date.parse(details[0]!.retryAfter)),
      "rebind_exact_recovery_pending",
    );
  }
  const current = await waitFor(
    ctx,
    "managed_cold_recovery_started",
    () => database.started(scenario.sessionId, ctx.hostId),
    (value) =>
      value.phase === "started" && value.binding_hash === candidate.bindingHash,
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
  return { old, current, candidate, ordinaryResumeDenied };
}
