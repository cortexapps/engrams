import { authorizeOAuth, waitFor } from "./bootstrap.ts";
import type { CallPlan } from "./call-plan.ts";
import { requireThat, RunnerFailure } from "./context.ts";
import {
  completeAssertions,
  type CaseDriver,
  type DriverAssertion,
} from "./driver.ts";
import { FixtureObserver } from "./observer.ts";
import { recoverCurrentBinding } from "./rebind.ts";
import {
  closeScenario,
  execution,
  pollProof,
  sendScenarioPrompt,
  startScenario,
  verifiedCounterResult,
} from "./scenario-runtime.ts";
import { assertNoDispatch, policyObservation } from "./case-grants.ts";

type Authority = {
  bindingHash: string;
  current: boolean;
  authGeneration: number;
  credentialIdentity: string;
};
export function assertStableAuthority(before: Authority, after: Authority) {
  requireThat(
    before.current &&
      after.current &&
      after.bindingHash === before.bindingHash &&
      after.authGeneration === before.authGeneration &&
      after.credentialIdentity === before.credentialIdentity,
    "same_oauth_authority_binding_stable",
  );
}
export function assertInvalidatedAuthority(
  oldHash: string,
  after: { bindingHash: string; current: boolean; staleReasons: string[] },
) {
  requireThat(
    after.bindingHash === oldHash &&
      !after.current &&
      after.staleReasons.length > 0,
    "changed_oauth_authority_requires_explicit_rebind",
  );
}
export function oauthCallPlan(alias: string): CallPlan {
  return {
    steps: [
      "initial",
      "rotation",
      "account",
      "account-rebound",
      "scope",
      "scope-rebound",
      "reconnect",
      "reconnect-rebound",
      "credential-probe",
    ].map((marker) => ({
      marker,
      tool: alias,
      arguments: {},
      callId: `oauth-${marker}`,
      resultFormat: "mcp",
    })),
  };
}

export const oauthAuthorityDriver: CaseDriver = async ({
  ctx,
  principals,
  database,
  harness,
}) => {
  const publicSetup = await principals.admin.integration.getMcpSetup({});
  const registrations = publicSetup.registrations.filter(
    (r) =>
      r.issuer === ctx.protocolFixtures.modern.issuerOrigin &&
      r.resource?.endpoint ===
        `${ctx.protocolFixtures.modern.resourceOrigin}/mcp` &&
      r.redirectUri === ctx.protocolFixtures.modern.redirectUri,
  );
  if (
    registrations.length !== 1 ||
    registrations[0]!.scopes.length !== 1 ||
    registrations[0]!.scopes[0] !== "tools:read"
  )
    throw new RunnerFailure(
      "NEEDS_CONTEXT",
      "oauth_fixture_registration_exact_read_scope",
    );
  const fixture = new FixtureObserver(ctx);
  const fixtureBefore = await fixture.observe();
  await fixture.configure({
    reply: "json",
    response: "normal",
    holdCalls: false,
    revision: 1,
  });
  await fixture.oauth({
    account: "account-one",
    scope: "tools:read",
    issuer: "configured",
    expiresInSeconds: 2,
  });
  try {
    const scenario = await startScenario(ctx, principals, database, {
      harness,
      suffix: `${harness}-oauth`,
      protocol: "modern",
      upstreamName: "read_counter",
      authKind: "oauth",
      mode: "read",
      caller: "owner",
      automationWrite: false,
      arguments: {},
      constraints: [],
      callId: "oauth-initial",
      callPlan: oauthCallPlan,
    });
    const assertions: DriverAssertion[] = [];
    try {
      const authority = async () => {
        const state = await principals.admin.integration.getSessionMcpState({
          sessionId: scenario.sessionId,
          omitExecutions: true,
          bindingPageSize: 16,
        });
        const setup = await principals.admin.integration.getMcpSetup({
          connectionId: scenario.connection.id,
        });
        requireThat(
          state.bindingReceipt && setup.current?.connection,
          "oauth_actual_binding_and_connection",
        );
        return {
          bindingHash: state.bindingReceipt.bindingHash,
          current: state.bindingReceipt.current,
          staleReasons: state.bindingReceipt.staleReasons,
          authGeneration: setup.current.connection.authGeneration,
          credentialIdentity: setup.current.connection.credentialIdentity,
        };
      };
      const successfulRead = async (marker: string) => {
        const before = await fixture.observe();
        await sendScenarioPrompt(scenario, false, marker);
        const call = await execution(ctx, scenario, true, `oauth-${marker}`);
        requireThat(call?.state === "succeeded", "oauth_actual_read_succeeded");
        const result = await verifiedCounterResult(
          scenario,
          call.executionId,
          before.effects.count,
          false,
        );
        const proofs = await pollProof(ctx, scenario, {
          callId: `oauth-${marker}`,
          tool: scenario.alias,
          planSha256: scenario.planSha256,
        });
        requireThat(
          proofs.length === 1 &&
            proofs[0]!.proof.outcome === "succeeded" &&
            proofs[0]!.proof.delivery === "available",
          "oauth_native_read_delivered",
        );
        const after = await fixture.observe();
        requireThat(
          after.reads === before.reads + 1 &&
            after.effects.executions === before.effects.executions &&
            after.effects.count === before.effects.count,
          "oauth_exact_read_no_write",
        );
        return {
          before,
          after,
          resultHash: result.resultHash,
          proofHash: proofs[0]!.sha256,
        };
      };
      const initial = await successfulRead("initial");
      requireThat(
        initial.after.oauth.authorizations ===
          fixtureBefore.oauth.authorizations + 1 &&
          initial.after.oauth.exchanges === fixtureBefore.oauth.exchanges + 1 &&
          initial.after.oauth.expiresInSeconds === 2,
        "real_oauth_pkce_exchange_and_short_expiry",
      );
      const setup = await principals.admin.integration.getMcpSetup({
        connectionId: scenario.connection.id,
      });
      const connection = setup.current?.connection;
      const registration = setup.registrations.find(
        (r) => r.id === connection?.mcp?.oauthClientRegistrationId,
      );
      requireThat(
        registration?.issuer === ctx.protocolFixtures.modern.issuerOrigin &&
          registration.resource?.endpoint ===
            `${ctx.protocolFixtures.modern.resourceOrigin}/mcp` &&
          registration.redirectUri === ctx.protocolFixtures.modern.redirectUri,
        "oauth_exact_resource_issuer_and_callback",
      );
      assertions.push(
        {
          assertion: "real-pkce",
          observations: [
            policyObservation(
              "fixture-observer",
              scenario.connection.id,
              "authorized",
              initial.after.oauth,
            ),
          ],
        },
        {
          assertion: "exact-resource-issuer",
          observations: [
            policyObservation(
              "public-rpc",
              scenario.connection.id,
              "authorized",
              {
                issuer: registration.issuer,
                resource: registration.resource.endpoint,
                callback: registration.redirectUri,
              },
            ),
          ],
        },
      );
      const beforeRotation = await authority();
      // Wait for the actual issued two-second token to expire. No clock or stored timestamp changes.
      await new Promise((resolve) => setTimeout(resolve, 2100));
      const rotation = await successfulRead("rotation");
      requireThat(
        rotation.after.oauth.rotations > rotation.before.oauth.rotations,
        "coordinator_expiry_refresh_rotation",
      );
      const afterRotation = await authority();
      assertStableAuthority(beforeRotation, afterRotation);
      assertions.push(
        {
          assertion: "expiry-rotation",
          observations: [
            policyObservation(
              "fixture-observer",
              scenario.connection.id,
              "succeeded",
              { before: rotation.before.oauth, after: rotation.after.oauth },
            ),
          ],
        },
        {
          assertion: "same-identity-binding-stable",
          observations: [
            policyObservation("public-rpc", scenario.sessionId, "current", {
              beforeRotation,
              afterRotation,
            }),
          ],
        },
      );
      for (const change of ["account", "scope", "reconnect"] as const) {
        const old = await authority();
        requireThat(old.current, "oauth_current_before_change");
        if (change === "reconnect") {
          const current = await principals.admin.integration.getMcpSetup({
            connectionId: scenario.connection.id,
          });
          requireThat(
            current.current?.connection,
            "oauth_disconnect_current_revision",
          );
          await principals.admin.integration.disconnectMcp({
            connectionId: scenario.connection.id,
            expectedRevision: current.current.connection.configRevision,
            expectedGeneration: current.current.connection.authGeneration,
          });
          await authorizeOAuth(ctx, principals.admin, scenario.connection.id);
        } else {
          await fixture.oauth(
            change === "account"
              ? { account: "account-two" }
              : { scope: "tools:read tools:write" },
          );
          await new Promise((resolve) => setTimeout(resolve, 2100));
        }
        const before = await fixture.observe();
        await sendScenarioPrompt(scenario, false, change);
        const proofs = await pollProof(ctx, scenario, {
          callId: `oauth-${change}`,
          tool: scenario.alias,
          planSha256: scenario.planSha256,
        });
        requireThat(
          proofs.length === 1 &&
            proofs[0]!.proof.resultReceived &&
            proofs[0]!.proof.outcome !== "succeeded",
          "changed_oauth_call_refused",
        );
        const stale = await waitFor(
          ctx,
          "oauth_binding_invalidated",
          authority,
          (value) => !value.current,
        );
        assertInvalidatedAuthority(old.bindingHash, stale);
        const after = await fixture.observe();
        assertNoDispatch(before, after);
        if (change !== "reconnect") {
          requireThat(
            after.oauth.rotations > before.oauth.rotations,
            "changed_authority_actual_refresh_attempt",
          );
          // The expanded response is refused. Restore the reviewed scope for a new explicit authorization.
          if (change === "scope") await fixture.oauth({ scope: "tools:read" });
          await authorizeOAuth(ctx, principals.admin, scenario.connection.id);
        }
        // The new credential requires a new reviewed catalog and an explicit human rebind.
        const current = await principals.admin.integration.getMcpSetup({
          connectionId: scenario.connection.id,
        });
        requireThat(
          current.current?.connection,
          "oauth_current_for_catalog_review",
        );
        const draft = await principals.admin.integration.discoverMcp({
          connectionId: scenario.connection.id,
          expectedRevision: current.current.connection.configRevision,
        });
        requireThat(draft.catalog, "oauth_reconnect_reviewed_draft");
        const catalogs = await principals.admin.integration.getMcpCatalogs({
          connectionId: scenario.connection.id,
        });
        await principals.admin.integration.publishMcp({
          connectionId: scenario.connection.id,
          draftId: draft.catalog.id,
          expectedRevision: catalogs.catalogRevision,
          tools: draft.catalog.tools.map((tool) => ({
            ...tool,
            classification:
              tool.upstreamName === "read_counter" ? "read" : "write",
            enabled: true,
          })),
        });
        const recovery = await recoverCurrentBinding(ctx, scenario, database);
        requireThat(
          recovery.old.binding_hash === old.bindingHash,
          "oauth_recovery_old_binding",
        );
        const fresh = await waitFor(
          ctx,
          "oauth_rebound_current",
          authority,
          (value) =>
            value.current &&
            value.bindingHash === recovery.candidate.bindingHash,
        );
        const read = await successfulRead(`${change}-rebound`);
        assertions.push({
          assertion:
            change === "account"
              ? "account-change-invalidates"
              : change === "scope"
                ? "scope-expansion-invalidates"
                : "reconnect",
          observations: [
            policyObservation("public-rpc", scenario.sessionId, "current", {
              old,
              stale,
              fresh,
              recovery: {
                oldBoot: recovery.old.boot_attempt,
                newBoot: recovery.current.boot_attempt,
                candidateHash: recovery.candidate.bindingHash,
              },
              resultHash: read.resultHash,
            }),
            policyObservation("guest-proof", scenario.sessionId, "denied", {
              hash: proofs[0]!.sha256,
            }),
            policyObservation(
              "fixture-observer",
              scenario.connection.id,
              "denied",
              { before, after },
              0,
            ),
          ],
        });
      }
      await fixture.configure({ holdCalls: true });
      await sendScenarioPrompt(scenario, false, "credential-probe");
      await waitFor(
        ctx,
        "oauth_probe_live_model",
        () => fixture.observe(),
        (value) => value.held === 1,
      );
      // The common helper observes both real processes and inline managed manifest, without printing secrets.
      const { probeOAuthCredentialAbsence } = await import("./guest.ts");
      const probe = await probeOAuthCredentialAbsence(
        ctx,
        scenario.client,
        scenario.sessionId,
        harness,
      );
      assertions.push({
        assertion: "no-credential-leak",
        observations: [
          policyObservation(
            "guest-proof",
            scenario.sessionId,
            "current",
            probe,
          ),
        ],
      });
      await fixture.release();
      const probeCall = await execution(
        ctx,
        scenario,
        true,
        "oauth-credential-probe",
      );
      requireThat(
        probeCall?.state === "succeeded",
        "oauth_probe_call_finished",
      );
      await pollProof(ctx, scenario, {
        callId: "oauth-credential-probe",
        tool: scenario.alias,
        planSha256: scenario.planSha256,
      });
      return completeAssertions("oauth-authority", assertions);
    } finally {
      await closeScenario(principals, scenario);
    }
  } finally {
    await fixture.release();
    await fixture.configure({ holdCalls: false });
    await fixture.oauth({
      account: "account-one",
      scope: "tools:read tools:write",
      issuer: "configured",
      expiresInSeconds: 3600,
    });
  }
};
