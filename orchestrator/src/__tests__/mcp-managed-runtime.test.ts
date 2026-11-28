import { expect, spyOn, test } from "bun:test";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import { SessionPolicyService } from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { makeManagedLifecycleStore } from "../mcp/managed-lifecycle-store.ts";
import { makeManagedSessionRuntime, McpManagedCreationPending } from "../mcp/managed-runtime.ts";
import { managedQuery, decodeManagedState } from "../mcp/managed-state.ts";
import { makeManagedPolicyAuthority } from "../mcp/managed-policy.ts";
import { reviewFixture } from "./mcp-review-fixture.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
import type { ManagedLifecycle } from "../mcp/managed-lifecycle.ts";
import { create } from "@bufbuild/protobuf";
import { createHash } from "node:crypto";
import {
  ManagedAppliedStateSchema,
  ManagedPolicyTargetSchema,
  type ManagedAppliedState,
} from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { managedPolicy } from "../mcp/managed-policy.ts";
import type { ManagedCreationState } from "../mcp/managed-state.ts";
import { operatorPublication } from "./mcp-binding-fixture.ts";
import * as controlClients from "../mcp/control-client.ts";
import * as bindingStore from "../mcp/binding-store.ts";
import { makeMcpRuntime } from "../mcp/runtime.ts";
import { McpAuthorityStale } from "../mcp/authority-error.ts";

// Real O persistence and generated in-process control transport; no physical boot is claimed.
test("lost reserve response retains immutable identity and never regenerates or resends secret input", async () => {
  await verifyMcpTestDatabases();
  const f = await reviewFixture();
  try {
    let reserves = 0,
      prepares = 0,
      enrollments = 0,
      exists = false;
    const store = makeManagedLifecycleStore(f.db);
    const control = createClient(
      SessionPolicyService,
      createRouterTransport((router) =>
        router.service(SessionPolicyService, {
          async reserveManagedCreate() {
            reserves++;
            throw Error("lost response");
          },
        }),
      ),
    );
    const unavailable = async (): Promise<never> => {
      throw Error("unexpected host operation");
    };
    const managed = makeManagedPolicyAuthority({
      control,
      readPlacement: unavailable,
      readPublished: unavailable,
    });
    const response = async (phase: string) => {
      const value = (await store.read(f.sessionId)).session.mcpManagedLifecycle!;
      return {
        protocol: 1,
        stateJson: new TextEncoder().encode(
          JSON.stringify({
            query: managedQuery(value),
            state: phase,
            boot: null,
            operation: null,
            terminal_reason: null,
          }),
        ),
      };
    };
    const runtime = makeManagedSessionRuntime({
      store,
      control,
      managed,
      readPublished: unavailable,
      authorize: async () => {},
      validate: async () => {},
      enroll: async () => {
        enrollments++;
      },
      enabled: () => true,
      bridge: {
        readManagedCreationState: () => response(exists ? "held" : "absent"),
        prepareManagedBoot: async () => {
          prepares++;
          return response("preparing");
        },
        recoverManagedBoot: unavailable,
      },
    });
    const input = {
      imageUri: "image",
      mode: "agent",
      requestedSessionId: f.sessionId,
      harnessEnv: { ENGRAM_TOOLS: "[]", PRIVATE_NATIVE_TOKEN: "neutral-secret-value" },
      mcpBindingEnvelope: (await store.read(f.sessionId)).session.mcpBindingEnvelope!,
      integrationConnections: (await store.read(f.sessionId)).session.integrationConnections!,
    };
    await expect(
      runtime.create(input, { actorId: f.actorId, inputId: f.sessionId }),
    ).rejects.toBeInstanceOf(McpManagedCreationPending);
    const durable = (await store.read(f.sessionId)).session.mcpManagedLifecycle!;
    expect(durable.phase).toBe("reserving");
    expect(JSON.stringify(durable)).not.toContain("neutral-secret-value");
    await runtime.runOnce(f.sessionId);
    expect(prepares).toBe(0);
    expect(enrollments).toBe(0);
    exists = true;
    await runtime.runOnce(f.sessionId);
    expect(prepares).toBe(1);
    expect(enrollments).toBe(1);
    await runtime.create(
      { ...input, harnessEnv: { ENGRAM_TOOLS: "[]", PRIVATE_NATIVE_TOKEN: "regenerated-value" } },
      { actorId: f.actorId, inputId: f.sessionId },
    );
    expect(reserves).toBe(1);
    expect((await store.read(f.sessionId)).session.mcpManagedLifecycle).toEqual(durable);
  } finally {
    await f.cleanup();
  }
});

test("managed creation state requires exact query, explicit presence and sanitized boot", () => {
  const value: ManagedLifecycle = {
    version: 1,
    identity: {
      sessionId: "session",
      taskId: "automation:run",
      inputId: "input",
      actorId: "automation:source",
      writeEligibility: "read_only",
      inputDigest: `sha256:${"a".repeat(64)}`,
    },
    nativeVersion: 1,
    nativeDigest: `sha256:${"b".repeat(64)}`,
    bindingHash: `sha256:${"c".repeat(64)}`,
    phase: "reserving",
  };
  const body = {
    query: managedQuery(value),
    state: "absent",
    boot: null,
    operation: null,
    terminal_reason: null,
  };
  const encode = (input: unknown) => ({
    protocol: 1,
    stateJson: new TextEncoder().encode(JSON.stringify(input)),
  });
  expect(decodeManagedState(encode(body), value).state).toBe("absent");
  expect(() =>
    decodeManagedState(
      encode({ ...body, query: { ...body.query, native_digest: `sha256:${"d".repeat(64)}` } }),
      value,
    ),
  ).toThrow("identity");
  expect(() => decodeManagedState({ protocol: 1, stateJson: new Uint8Array() }, value)).toThrow(
    "unavailable",
  );
  expect(() =>
    decodeManagedState(encode({ ...body, boot: { sealed_agent: { ciphertext: [1] } } }), value),
  ).toThrow();
});

for (const missingProof of [false, true])
  test(`final boot requires real receipt shape and durable exact manifest before start (missing proof ${missingProof})`, async () => {
    await verifyMcpTestDatabases();
    const f = await reviewFixture();
    try {
      const store = makeManagedLifecycleStore(f.db);
      let state: ManagedCreationState | undefined, applied: ManagedAppliedState | undefined;
      let finalizeCalls = 0,
        applyCalls = 0;
      const physicalNetworkIdentity = `sha256:${"8".repeat(64)}`;
      const publication = {
        generation: 1,
        identity: operatorPublication.identity,
        policies: f.authority.operatorPolicies,
      };
      const target = create(ManagedPolicyTargetSchema, {
        sessionId: f.sessionId,
        sandboxId: crypto.randomUUID(),
        hostId: crypto.randomUUID(),
        bindingEpoch: 1n,
      });
      const targetJson = {
        session_id: f.sessionId,
        sandbox_id: target.sandboxId,
        host_id: target.hostId,
        binding_epoch: 1,
      };
      const control = createClient(
        SessionPolicyService,
        createRouterTransport((router) =>
          router.service(SessionPolicyService, {
            async reserveManagedCreate() {
              const v = (await store.read(f.sessionId)).session.mcpManagedLifecycle!;
              state = {
                query: managedQuery(v),
                state: "held",
                boot: null,
                operation: null,
                terminal_reason: null,
              };
              return { protocol: 1, identity: v.identity, state: "held" };
            },
            async readNativeManifest() {
              const v = (await store.read(f.sessionId)).session.mcpManagedLifecycle!;
              return {
                protocol: 1,
                identity: v.identity,
                nativeVersion: 1,
                nativeDigest: v.nativeDigest,
                nativeBytes: new TextEncoder().encode("[]"),
              };
            },
            async readManagedPolicy() {
              return {
                protocol: 1,
                retainedDesired: {
                  target,
                  policy: applied?.policy ?? {
                    ...f.snapshot.managedDenyPolicy,
                    revision: BigInt(f.snapshot.managedDenyPolicy.revision),
                  },
                },
                applied,
              };
            },
            async applyManagedPolicy(request) {
              applyCalls++;
              const v = (await store.read(f.sessionId)).session.mcpManagedLifecycle!;
              expect(v.phase).toBe("finalizing");
              expect(v.pending?.snapshot.bindingHash).toBe(request.context!.bindingHash!);
              const p = managedPolicy(
                create(
                  (await import("../gen/engram/mcp/v1/session_policy_pb.ts"))
                    .ManagedDenyPolicySchema,
                  request.policy!,
                ),
              );
              const json = {
                context: {
                  target: targetJson,
                  operator_generation: 1,
                  operator_identity: publication.identity,
                  binding_hash: request.context!.bindingHash,
                },
                policy: p,
                host_incarnation: "host-incarnation",
                proxy_incarnation: "proxy-incarnation",
                state_id: "",
                network_proof: missingProof
                  ? null
                  : {
                      target: targetJson,
                      guest_ip: "192.0.2.6",
                      network_identity: physicalNetworkIdentity,
                    },
              };
              applied = create(ManagedAppliedStateSchema, {
                context: request.context,
                policy: request.policy,
                hostIncarnation: json.host_incarnation,
                proxyIncarnation: json.proxy_incarnation,
                stateId: `sha256:${createHash("sha256").update(JSON.stringify(json)).digest("hex")}`,
                ...(missingProof
                  ? {}
                  : {
                      networkProof: {
                        target,
                        guestIp: "192.0.2.6",
                        networkIdentity: physicalNetworkIdentity,
                      },
                    }),
              });
              return { protocol: 1, applied };
            },
            async finalizeManagedBoot(request) {
              finalizeCalls++;
              const input: {
                native_version: 1;
                combined_manifest: number[];
                combined_manifest_digest: string;
              } = JSON.parse(new TextDecoder().decode(request.inputJson));
              expect(applied).toBeDefined();
              expect(
                (await store.read(f.sessionId)).session.mcpManagedLifecycle!.pending!
                  .combinedManifestDigest,
              ).toBe(input.combined_manifest_digest);
              state!.boot!.phase = "finalizing";
              state!.boot!.finalization = { ...input, prepared: state!.boot!.prepared! };
              throw Error("lost finalization reply");
            },
          }),
        ),
      );
      const managed = makeManagedPolicyAuthority({
        control,
        readPublished: async () => publication,
        readPlacement: async () => {
          const prepared = state?.boot?.prepared;
          if (!prepared) throw Error("MCP managed placement unavailable");
          return {
            target,
            networkIdentity: prepared.network_identity,
            guestIp: prepared.guest_ip,
            hostIncarnation: prepared.host_incarnation,
          };
        },
      });
      const encode = () => ({
        protocol: 1,
        stateJson: new TextEncoder().encode(JSON.stringify(state)),
      });
      const runtime = makeManagedSessionRuntime({
        store,
        control,
        managed,
        enabled: () => true,
        authorize: async () => {},
        validate: async () => {},
        enroll: async () => {},
        readPublished: async () => publication,
        bridge: {
          readManagedCreationState: async () => encode(),
          recoverManagedBoot: async () => {
            throw Error("unexpected recovery");
          },
          async prepareManagedBoot() {
            const v = (await store.read(f.sessionId)).session.mcpManagedLifecycle!;
            const request = {
              key: {
                creation: state!.query.identity,
                native_digest: v.nativeDigest,
                boot_attempt: "boot-attempt",
                context: {
                  target: targetJson,
                  operator_generation: 1,
                  operator_identity: publication.identity,
                  binding_hash: null,
                },
              },
              bootstrap_policy: f.snapshot.managedDenyPolicy,
            };
            state!.state = "boot";
            state!.boot = {
              request,
              revision: 2,
              phase: "prepared",
              sealed_agent: null,
              finalization: null,
              retired: null,
              prepared: {
                request,
                host_incarnation: "host-incarnation",
                network_identity: physicalNetworkIdentity,
                guest_ip: "192.0.2.6",
                receipt_id: `sha256:${"a".repeat(64)}`,
              },
            };
            return encode();
          },
        },
      });
      const row = (await store.read(f.sessionId)).session;
      await runtime.create(
        {
          imageUri: "image",
          mode: "agent",
          requestedSessionId: f.sessionId,
          harnessEnv: { ENGRAM_TOOLS: "[]" },
          mcpBindingEnvelope: row.mcpBindingEnvelope!,
          integrationConnections: row.integrationConnections!,
        },
        { actorId: f.actorId, inputId: f.sessionId },
      );
      await expect(runtime.runOnce(f.sessionId)).rejects.toThrow();
      expect(applyCalls).toBe(1);
      expect(finalizeCalls).toBe(missingProof ? 0 : 1);
      expect((await store.read(f.sessionId)).session.mcpManagedLifecycle!.phase).toBe("finalizing");
      if (!missingProof) {
        state!.boot!.phase = "started";
        const tls = {
          url: "https://control.example.test/",
          ca: "ca",
          cert: "cert",
          key: "key",
          serverFingerprint: Array(32).fill("AA").join(":"),
        };
        const clientSpy = spyOn(controlClients, "makeSessionPolicyControlClient").mockReturnValue(
          control,
        );
        const storedSpy = spyOn(bindingStore, "readStoredBindingSession").mockRejectedValue(
          Error("stored binding reached"),
        );
        try {
          const composed = makeMcpRuntime({
            db: f.db,
            deploymentId: f.deploymentId,
            enabled: () => true,
            configuration: {
              infrastructure: operatorPublication,
              control: tls,
              registrations: [],
              workers: new Map(),
            },
            bridge: () => ({
              readManagedCreationState: async () => encode(),
              prepareManagedBoot: async () => encode(),
              recoverManagedBoot: async () => encode(),
            }),
          });
          if (!composed.resolver) throw Error("configured resolver missing");
          const error = await composed.resolver.readCurrent(f.sessionId).catch((e: unknown) => e);
          expect(error).toBeInstanceOf(Error);
          expect(error).not.toBeInstanceOf(McpAuthorityStale);
          expect(error).toHaveProperty("message", "MCP managed session start is pending");
          expect(storedSpy).not.toHaveBeenCalled();
          const originalKey = state!.boot!.request.key;
          state!.boot!.request.key = {
            ...originalKey,
            boot_attempt: "other-attempt",
          };
          await expect(composed.resolver.readCurrent(f.sessionId)).rejects.toBeInstanceOf(
            McpAuthorityStale,
          );
          state!.boot!.request.key = originalKey;
          await runtime.runOnce(f.sessionId);
          expect(finalizeCalls).toBe(1);
          expect(applyCalls).toBe(1);
          expect((await store.read(f.sessionId)).session.mcpManagedLifecycle!.phase).toBe(
            "started",
          );
          await expect(composed.resolver.readCurrent(f.sessionId)).rejects.toThrow(
            "stored binding reached",
          );
        } finally {
          storedSpy.mockRestore();
          clientSpy.mockRestore();
        }
      }
    } finally {
      await f.cleanup();
    }
  });
