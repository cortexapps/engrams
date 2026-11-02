import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { create } from "@bufbuild/protobuf";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import {
  ManagedAppliedStateSchema,
  ManagedPolicyTargetSchema,
  ManagedPolicyPhase,
  SessionPolicyService,
} from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { canonicalDigest } from "../mcp/hash.ts";
import {
  makeManagedPolicyAuthority,
  managedAppliedJson,
  unionManagedPolicies,
} from "../mcp/managed-policy.ts";
import { operatorPublication } from "./mcp-binding-fixture.ts";
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
test("fresh final host receipt requires explicit retained presence, exact target, binding and network", async () => {
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
      network_identity: operatorPublication.networkIdentity,
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
      networkIdentity: operatorPublication.networkIdentity,
    },
  });
  expect(managedAppliedJson(applied).state_id).toBe(applied.stateId);
  let present = true,
    stale = false;
  const control = createClient(
    SessionPolicyService,
    createRouterTransport((router) =>
      router.service(SessionPolicyService, {
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
    readTarget: async () =>
      stale ? create(ManagedPolicyTargetSchema, { ...target, bindingEpoch: 3n }) : target,
    readPublished: async () => ({
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
    }),
  });
  const expected = {
    sessionId: target.sessionId,
    bindingEpoch: 2,
    bindingHash,
    denyRevision: 3,
    denyDigest: deny.digest,
  };
  expect(await authority.enforce(expected, deny)).toEqual(expected);
  present = false;
  await expect(authority.enforce(expected, deny)).rejects.toThrow("presence");
  present = true;
  stale = true;
  await expect(authority.enforce(expected, deny)).rejects.toThrow("binding");
  stale = false;
  await expect(
    authority.enforce({ ...expected, bindingHash: operatorPublication.identity }, deny),
  ).rejects.toThrow("acknowledgement");
  applied.networkProof!.networkIdentity = `sha256:${"9".repeat(64)}`;
  await expect(authority.enforce(expected, deny)).rejects.toThrow("digest");
});
