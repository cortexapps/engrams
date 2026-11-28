import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { create } from "@bufbuild/protobuf";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import {
  ManagedAppliedStateSchema,
  ManagedPolicyTargetSchema,
  ManagedPolicyPhase,
  SessionPolicyService,
  type ManagedAppliedState,
} from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { canonicalDigest } from "../mcp/hash.ts";
import {
  makeManagedPolicyAuthority,
  managedAppliedJson,
  unionManagedPolicies,
} from "../mcp/managed-policy.ts";
import { operatorPublication } from "./mcp-binding-fixture.ts";
const physicalNetworkIdentity = `sha256:${"8".repeat(64)}`;
const policy = (revision: number, hosts: string[]) => {
  const body = { version: 1 as const, revision, hosts };
  return { ...body, digest: canonicalDigest(body) };
};

test("managed deny union keeps orphaned applied and retained desired authority", () => {
  expect(
    unionManagedPolicies([
      policy(2, ["old.example.test"]),
      policy(7, ["orphan.example.test"]),
      policy(9, ["retained.example.test"]),
      policy(3, ["next.example.test"]),
    ]),
  ).toEqual(
    policy(10, [
      "next.example.test",
      "old.example.test",
      "orphan.example.test",
      "retained.example.test",
    ]),
  );
});
function fixture() {
  const target = create(ManagedPolicyTargetSchema, {
    sessionId: crypto.randomUUID(),
    sandboxId: crypto.randomUUID(),
    hostId: crypto.randomUUID(),
    bindingEpoch: 2n,
  });
  const deny = policy(3, ["tools.example.test"]),
    bindingHash = `sha256:${"a".repeat(64)}`;
  const targetJson = {
    session_id: target.sessionId,
    sandbox_id: target.sandboxId,
    host_id: target.hostId,
    binding_epoch: 2,
  };
  const json = {
    context: {
      target: targetJson,
      operator_generation: 1,
      operator_identity: operatorPublication.identity,
      binding_hash: bindingHash,
    },
    policy: deny,
    host_incarnation: "host-incarnation",
    proxy_incarnation: "proxy-incarnation",
    state_id: "",
    network_proof: {
      target: targetJson,
      guest_ip: "192.0.2.5",
      network_identity: physicalNetworkIdentity,
    },
  };
  const applied = create(ManagedAppliedStateSchema, {
    context: {
      target,
      operatorGeneration: 1n,
      operatorIdentity: operatorPublication.identity,
      phase: ManagedPolicyPhase.FINAL,
      bindingHash,
    },
    policy: { ...deny, revision: 3n },
    hostIncarnation: json.host_incarnation,
    proxyIncarnation: json.proxy_incarnation,
    stateId: `sha256:${createHash("sha256").update(JSON.stringify(json)).digest("hex")}`,
    networkProof: {
      target,
      guestIp: "192.0.2.5",
      networkIdentity: physicalNetworkIdentity,
    },
  });
  const placement = {
    target,
    networkIdentity: physicalNetworkIdentity,
    guestIp: "192.0.2.5",
    hostIncarnation: "host-incarnation",
  };
  let present = true;
  const publication = {
    generation: 1,
    identity: operatorPublication.identity,
    policies: [
      {
        publication: operatorPublication,
        zoneId: "public",
        policyRevision: "1",
        policyDigest: operatorPublication.identity,
        authorities: ["tools.example.test"],
        aliases: [],
      },
    ],
  };
  const control = createClient(
    SessionPolicyService,
    createRouterTransport((router) =>
      router.service(SessionPolicyService, {
        applyManagedPolicy: async () => ({ protocol: 1, applied }),
        readManagedPolicy: async () => ({
          protocol: 1,
          applied,
          ...(present ? { retainedDesired: { target, policy: { ...deny, revision: 3n } } } : {}),
        }),
      }),
    ),
  );
  const authority = makeManagedPolicyAuthority({
    control,
    readPlacement: async () => placement,
    readPublished: async () => publication,
  });
  const expected = {
    sessionId: target.sessionId,
    bindingEpoch: 2,
    bindingHash,
    denyRevision: 3,
    denyDigest: deny.digest,
  };
  return {
    target,
    deny,
    bindingHash,
    applied,
    placement,
    publication,
    authority,
    expected,
    omitRetained: () => {
      present = false;
    },
  };
}
function resign(applied: ManagedAppliedState) {
  const target = applied.context!.target!;
  const targetJson = {
    session_id: target.sessionId,
    sandbox_id: target.sandboxId,
    host_id: target.hostId,
    binding_epoch: Number(target.bindingEpoch),
  };
  applied.stateId = `sha256:${createHash("sha256")
    .update(
      JSON.stringify({
        context: {
          target: targetJson,
          operator_generation: Number(applied.context!.operatorGeneration),
          operator_identity: applied.context!.operatorIdentity,
          binding_hash: applied.context!.bindingHash,
        },
        policy: policy(Number(applied.policy!.revision), applied.policy!.hosts),
        host_incarnation: applied.hostIncarnation,
        proxy_incarnation: applied.proxyIncarnation,
        state_id: "",
        network_proof: applied.networkProof
          ? {
              target: targetJson,
              guest_ip: applied.networkProof.guestIp,
              network_identity: applied.networkProof.networkIdentity,
            }
          : null,
      }),
    )
    .digest("hex")}`;
}

for (const operation of ["apply", "enforce"] as const) {
  const run = (f: ReturnType<typeof fixture>) =>
    operation === "apply"
      ? f.authority.apply(f.target, f.bindingHash, f.deny, undefined, f.publication)
      : f.authority.enforce(f.expected, f.deny);
  test(`${operation} accepts current prepared physical identity distinct from publication`, async () => {
    const f = fixture();
    expect(f.placement.networkIdentity).not.toBe(operatorPublication.networkIdentity);
    expect(managedAppliedJson(f.applied).state_id).toBe(f.applied.stateId);
    expect(await run(f)).toEqual(operation === "apply" ? f.applied : f.expected);
  });
  for (const field of ["networkIdentity", "guestIp", "hostIncarnation"] as const)
    test(`${operation} rejects a re-digested same-target receipt with transplanted ${field}`, async () => {
      const f = fixture();
      if (field === "hostIncarnation") f.applied.hostIncarnation = "other-host-incarnation";
      else
        f.applied.networkProof![field] =
          field === "guestIp" ? "192.0.2.8" : operatorPublication.networkIdentity;
      resign(f.applied);
      expect(managedAppliedJson(f.applied).state_id).toBe(f.applied.stateId);
      await expect(run(f)).rejects.toThrow(
        operation === "apply"
          ? "MCP managed apply changed"
          : "MCP final managed acknowledgement unavailable",
      );
    });
  for (const field of ["sandboxId", "hostId", "bindingEpoch"] as const)
    test(`${operation} rejects current placement with stale ${field}`, async () => {
      const f = fixture();
      f.placement.target = create(ManagedPolicyTargetSchema, {
        ...f.target,
        [field]: field === "bindingEpoch" ? 3n : "other-placement",
      });
      await expect(run(f)).rejects.toThrow();
    });
  test(`${operation} rejects absent network proof`, async () => {
    const f = fixture();
    f.applied.networkProof = undefined;
    resign(f.applied);
    await expect(run(f)).rejects.toThrow("MCP managed receipt unavailable");
  });
  test(`${operation} rejects receipt with invalid digest`, async () => {
    const f = fixture();
    f.applied.networkProof!.networkIdentity = `sha256:${"9".repeat(64)}`;
    await expect(run(f)).rejects.toThrow("digest");
  });
  for (const field of ["operatorGeneration", "operatorIdentity", "bindingHash"] as const)
    test(`${operation} rejects re-digested receipt with changed ${field}`, async () => {
      const f = fixture();
      if (field === "operatorGeneration") f.applied.context![field] = 2n;
      else f.applied.context![field] = `sha256:${"9".repeat(64)}`;
      resign(f.applied);
      await expect(run(f)).rejects.toThrow(
        operation === "apply"
          ? "MCP managed apply changed"
          : "MCP final managed acknowledgement unavailable",
      );
    });
}
test("fresh final host receipt requires explicit retained presence and binding", async () => {
  const f = fixture();
  f.omitRetained();
  await expect(f.authority.enforce(f.expected, f.deny)).rejects.toThrow("presence");
  const current = fixture();
  await expect(
    current.authority.enforce(
      { ...current.expected, bindingHash: operatorPublication.identity },
      current.deny,
    ),
  ).rejects.toThrow("acknowledgement");
});
