import { waitFor } from "./bootstrap.ts";
import { requireThat, sha256 } from "./context.ts";
import {
  completeAssertions,
  type CaseDriver,
  type DriverObservation,
} from "./driver.ts";
import {
  canonicalJson,
  expectedCounterNative,
  probeOAuthCredentialAbsence,
} from "./guest.ts";
import {
  guestColdObservation,
  probeManagedAuthority,
  probeWorkerControl,
} from "./guest-physical.ts";
import {
  HostObserver,
  verifyColdChain,
  verifyPhysicalBoot,
} from "./host-observer.ts";
import { FixtureObserver } from "./observer.ts";
import { recoverCurrentBinding, revokeProfileGrants } from "./rebind.ts";
import {
  closeScenario,
  delivered,
  execution,
  pollProof,
  sendScenarioPrompt,
  startScenario,
  verifiedCounterResult,
} from "./scenario-runtime.ts";

function observe(
  identity: string,
  source: DriverObservation["source"],
  state: DriverObservation["state"],
  value: unknown,
): DriverObservation {
  return {
    source,
    state,
    identitySha256: sha256(identity),
    contentSha256: sha256(canonicalJson(value)),
  };
}

export function selectColdDenial(
  positive: Awaited<ReturnType<typeof pollProof>>,
  records: Awaited<ReturnType<typeof pollProof>>,
) {
  requireThat(positive.length === 1, "cold_positive_proof_count");
  const original = positive[0]!;
  requireThat(
    original.proof.resultReceived === true &&
      original.proof.delivery === "available" &&
      original.proof.outcome === "succeeded" &&
      original.proof.isError === false &&
      original.proof.planSha256 !== undefined,
    "cold_positive_proof_success",
  );
  // Raw JSONL hashes identify retained records regardless of launch filename order.
  requireThat(
    records.length === positive.length + 1 &&
      records.filter((record) => record.sha256 === original.sha256).length ===
        1,
    "cold_positive_proof_retained",
  );
  const denied = records.filter((record) => record.sha256 !== original.sha256);
  requireThat(
    denied.length === 1 &&
      denied.every(
        ({ proof }) =>
          proof.callId === original.proof.callId &&
          proof.tool === original.proof.tool &&
          proof.planSha256 === original.proof.planSha256 &&
          proof.expectExposure === "absent" &&
          proof.protocolDenialCode === "unknown_tool" &&
          proof.requestAttempted === true &&
          proof.exposedMcpToolNamesSha256 ===
            `sha256:${sha256(canonicalJson([]))}`,
      ),
    "cold_old_execution_actual_denial",
  );
  return denied;
}

export const credentialNetworkIsolationDriver: CaseDriver = async ({
  ctx,
  principals,
  database,
  harness,
}) => {
  const fixture = new FixtureObserver(ctx);
  await fixture.configure({
    reply: "json",
    response: "normal",
    holdCalls: true,
    revision: 1,
  });
  const before = await fixture.observe();
  const scenario = await startScenario(ctx, principals, database, {
    harness,
    suffix: `${harness}-physical-network`,
    protocol: "modern",
    upstreamName: "read_counter",
    mode: "read",
    caller: "owner",
    automationWrite: false,
    arguments: {},
    constraints: [],
    callId: "physical-read",
    defaultAllow: true,
    authKind: "oauth",
    expectedNativeResult: expectedCounterNative(
      harness,
      before.effects.count,
      false,
    ),
  });
  try {
    await sendScenarioPrompt(scenario);
    await waitFor(
      ctx,
      "physical_actual_call_held",
      () => fixture.observe(),
      (v) => v.held === before.held + 1,
    );
    const credentials = await probeOAuthCredentialAbsence(
      ctx,
      scenario.client,
      scenario.sessionId,
    );
    const host = new HostObserver(ctx);
    const baseline = await host.logBaseline();
    const probe = await probeManagedAuthority(
      ctx,
      scenario.client,
      scenario.sessionId,
    );
    const rejected = await waitFor(
      ctx,
      "actual_managed_proxy_rejection",
      () => host.proxyDenials(baseline, scenario.sessionId, probe.sni),
      (v) => v.length > 0,
    );
    const tls = await probeWorkerControl(ctx);
    await fixture.release();
    const row = await execution(ctx, scenario);
    requireThat(row?.state === "succeeded", "physical_guest_return_call");
    const result = await verifiedCounterResult(
      scenario,
      row.executionId,
      before.effects.count,
      false,
    );
    const completion = await delivered(ctx, database, scenario);
    const after = await fixture.observe();
    requireThat(
      after.reads === before.reads + 1 &&
        after.effects.executions === before.effects.executions &&
        after.effects.count === before.effects.count,
      "physical_network_no_extra_provider_call",
    );
    const identity = scenario.sessionId;
    const denial = observe(identity, "host-read-only", "denied", rejected);
    return completeAssertions("credential-network-isolation", [
      {
        assertion: "no-credential-in-guest",
        observations: [
          observe(identity, "guest-proof", "current", credentials),
        ],
      },
      {
        assertion: "default-allow-managed-authority-denied",
        observations: [denial],
      },
      {
        assertion: "worker-control-denied",
        observations: [observe(identity, "fixture-observer", "denied", tls)],
      },
      { assertion: "positive-network-denial", observations: [denial] },
      {
        assertion: "guest-return",
        observations: [
          observe(identity, "guest-proof", "succeeded", completion),
          observe(identity, "public-rpc", "succeeded", result),
          observe(identity, "fixture-observer", "current", after),
        ],
      },
    ]);
  } finally {
    await fixture.release();
    await fixture.configure({ holdCalls: false });
    await closeScenario(principals, scenario);
  }
};

export const diskOnlyColdRebindDriver: CaseDriver = async ({
  ctx,
  principals,
  database,
  harness,
}) => {
  const fixture = new FixtureObserver(ctx);
  await fixture.configure({
    reply: "json",
    response: "normal",
    holdCalls: false,
    revision: 1,
  });
  const before = await fixture.observe();
  const scenario = await startScenario(ctx, principals, database, {
    harness,
    suffix: `${harness}-physical-cold`,
    protocol: "modern",
    upstreamName: "read_counter",
    mode: "read",
    caller: "owner",
    automationWrite: false,
    arguments: {},
    constraints: [],
    callId: "cold-old-read",
    callPlan: (alias) => ({
      steps: [
        {
          marker: "before",
          tool: alias,
          arguments: {},
          callId: "cold-old-read",
          resultFormat: "mcp",
          expectedResult: expectedCounterNative(
            harness,
            before.effects.count,
            false,
          ),
        },
        {
          marker: "after",
          tool: alias,
          arguments: {},
          callId: "cold-old-read",
          resultFormat: "mcp",
          expectExposure: "absent",
        },
      ],
    }),
  });
  try {
    await sendScenarioPrompt(scenario, false, "before");
    const row = await execution(ctx, scenario);
    requireThat(row?.state === "succeeded", "cold_old_actual_execution");
    await delivered(ctx, database, scenario);
    const positive = await pollProof(ctx, scenario, {
      callId: scenario.input.callId,
      tool: scenario.alias,
      planSha256: scenario.planSha256,
    });
    const host = new HostObserver(ctx);
    const oldHost = await host.boot(
      scenario.coordinator.source_key.context.target.sandbox_id,
    );
    const oldAssets = await database.physicalBoot(
      scenario.sessionId,
      scenario.coordinator.boot_attempt,
    );
    const oldPhysical = verifyPhysicalBoot(
      ctx,
      oldHost.record,
      scenario.coordinator,
      oldAssets,
    );
    const oldGuest = await guestColdObservation(
      ctx,
      scenario.client,
      scenario.sessionId,
      true,
    );
    requireThat(
      oldGuest.manifestSha256 === oldPhysical.combinedSha256,
      "old_actual_live_frozen_manifest",
    );
    await revokeProfileGrants(principals.admin, scenario.profileId);
    const recovery = await recoverCurrentBinding(ctx, scenario, database);
    requireThat(
      recovery.candidate.bindingTools.length === 0,
      "physical_new_empty_binding",
    );
    const captured = await host.boot(
      recovery.old.source_key.context.target.sandbox_id,
    );
    const currentHost = await host.boot(
      recovery.current.source_key.context.target.sandbox_id,
    );
    const chain = verifyColdChain(captured.record, currentHost.record);
    const disk = await host.captureFile(captured.record);
    const currentAssets = await database.physicalBoot(
      scenario.sessionId,
      recovery.current.boot_attempt,
    );
    const currentPhysical = verifyPhysicalBoot(
      ctx,
      currentHost.record,
      recovery.current,
      currentAssets,
    );
    const currentGuest = await guestColdObservation(
      ctx,
      scenario.client,
      scenario.sessionId,
      false,
    );
    requireThat(
      currentGuest.bootId !== oldGuest.bootId &&
        currentGuest.manifestSha256 === currentPhysical.combinedSha256 &&
        oldPhysical.nativeSha256 === currentPhysical.nativeSha256 &&
        oldPhysical.combinedSha256 !== currentPhysical.combinedSha256 &&
        oldPhysical.combinedCount === oldPhysical.nativeCount + 1 &&
        currentPhysical.combinedCount === currentPhysical.nativeCount,
      "actual_cold_guest_and_frozen_bytes",
    );
    await sendScenarioPrompt(scenario, false, "after");
    const records = await pollProof(ctx, scenario, {
      callId: scenario.input.callId,
      tool: scenario.alias,
      planSha256: scenario.planSha256,
      afterCount: positive.length,
    });
    const denied = selectColdDenial(positive, records);
    const after = await fixture.observe();
    requireThat(
      after.reads === before.reads + 1 &&
        after.effects.executions === before.effects.executions &&
        after.effects.count === before.effects.count,
      "cold_no_old_provider_reexecution",
    );
    const currentRow = await execution(ctx, scenario);
    requireThat(
      currentRow?.executionId === row.executionId &&
        currentRow.state === "succeeded",
      "cold_no_replacement_execution",
    );
    const identity = scenario.sessionId;
    const cold = observe(identity, "host-read-only", "retired", {
      chain,
      disk,
      old: captured.sha256,
      current: currentHost.sha256,
    });
    const guest = observe(identity, "guest-proof", "started", {
      old: oldGuest,
      current: currentGuest,
    });
    return completeAssertions("disk-only-cold-rebind", [
      { assertion: "disk-only-restore", observations: [cold, guest] },
      {
        assertion: "new-native-and-mcp-frozen-bytes",
        observations: [
          observe(identity, "coordinator-read-only", "started", {
            old: oldPhysical,
            current: currentPhysical,
          }),
          guest,
        ],
      },
      {
        assertion: "old-target-retired-before-spawn",
        observations: [
          cold,
          observe(identity, "coordinator-read-only", "started", recovery),
        ],
      },
      { assertion: "no-old-ram", observations: [guest] },
      {
        assertion: "no-old-execution",
        observations: [
          observe(identity, "guest-proof", "denied", { positive, denied }),
          observe(identity, "fixture-observer", "current", after),
        ],
      },
    ]);
  } finally {
    await closeScenario(principals, scenario);
  }
};
