import { expect, spyOn, test } from "bun:test";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import { fromBinary } from "@bufbuild/protobuf";
import { createHash } from "node:crypto";
import { eq } from "drizzle-orm";
import { ZonePolicy } from "../../../mcp-worker/src/network.ts";
import { makeProfileStore } from "../db/profiles.ts";
import { sessionListener, task, taskSession } from "../db/schema.ts";
import {
  CreatePolicySessionRequestSchema,
  SessionPolicyService,
} from "../gen/engram/mcp/v1/session_policy_pb.ts";
import * as controlClients from "../mcp/control-client.ts";
import * as capabilities from "../mcp/session-capabilities.ts";
import { canonicalJson } from "../mcp/hash.ts";
import { McpManagedCreationPending } from "../mcp/managed-runtime.ts";
import { managedQuery } from "../mcp/managed-state.ts";
import { makeMcpRuntime } from "../mcp/runtime.ts";
import {
  createSessionForExistingTask,
  createTaskWithSession,
  registerSessionListener,
  type CreateTaskDeps,
} from "../rpc/task-create.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
import { reviewFixture } from "./mcp-review-fixture.ts";

await verifyMcpTestDatabases();

for (const existing of [false, true]) {
  for (const lostReply of [false, true]) {
    test(`public ${existing ? "existing-task" : "root"} managed create retains one listener (${lostReply ? "lost reserve reply" : "successful reserve"})`, async () => {
      const f = await reviewFixture();
      const taskId = existing ? f.taskId : crypto.randomUUID();
      const sessionId = crypto.randomUUID();
      let reserves = 0;
      let enabled = true;
      const deleted: string[] = [];
      const hash = (text: string) => `sha256:${createHash("sha256").update(text).digest("hex")}`;
      const zone = {
        id: "public",
        revision: "1",
        mode: "public" as const,
        authorities: ["tools.example.test"],
        aliases: [],
        cidrs: [],
        denyCidrs: [],
      };
      const infrastructure = {
        deploymentIdentity: hash("deployment"),
        networkIdentity: hash("network"),
        trustIdentity: hash("trust"),
        caContentDigests: { control: hash("ca") },
      };
      const candidate = {
        version: 1,
        zones: {
          public: {
            revision: "1",
            worker_digest: `sha256:${new ZonePolicy(zone).digest}`,
            authorities: zone.authorities,
            aliases: zone.aliases,
            configuration: JSON.parse(canonicalJson(zone)),
          },
        },
        deployment_identity: infrastructure.deploymentIdentity,
        network_identity: infrastructure.networkIdentity,
        trust_identity: infrastructure.trustIdentity,
        ca_content_digests: infrastructure.caContentDigests,
      };
      const control = createClient(
        SessionPolicyService,
        createRouterTransport((router) => {
          router.service(SessionPolicyService, {
            readPublishedOperator: async () => ({
              protocol: 1,
              publicationJson: new TextEncoder().encode(
                JSON.stringify({
                  published: {
                    generation: 1,
                    expected_generation: null,
                    identity: hash(JSON.stringify(candidate)),
                    candidate,
                  },
                  pending: null,
                  admission_open: true,
                }),
              ),
            }),
            reserveManagedCreate: async (request) => {
              reserves++;
              const input = fromBinary(CreatePolicySessionRequestSchema, request.creationRequest);
              expect(input.create?.requestedSessionId).toBe(sessionId);
              if (lostReply) throw Error("reserve reply lost");
              return {
                protocol: 1,
                state: "held",
                identity: {
                  sessionId,
                  taskId: request.taskId,
                  actorId: input.actorId,
                  inputId: input.inputId,
                  parentSessionId: input.parentSessionId,
                  inputDigest: request.inputDigest,
                  writeEligibility: "write_eligible",
                },
              };
            },
            readManagedCreationState: async () => {
              const [row] = await f.db
                .select()
                .from(taskSession)
                .where(eq(taskSession.sessionId, sessionId));
              if (!row?.mcpManagedLifecycle) throw Error("managed reservation missing");
              return {
                protocol: 1,
                stateJson: new TextEncoder().encode(
                  JSON.stringify({
                    query: managedQuery(row.mcpManagedLifecycle),
                    state: "held",
                    boot: null,
                    operation: null,
                    terminal_reason: null,
                  }),
                ),
              };
            },
          });
        }),
      );
      // Only the physical control transport is replaced; enrollment uses the real runtime and PG.
      const controlSpy = spyOn(controlClients, "makeSessionPolicyControlClient").mockReturnValue(
        control,
      );
      const tls = {
        url: "https://control.example.test/",
        ca: "ca",
        cert: "cert",
        key: "key",
        serverFingerprint: Array(32).fill("AA").join(":"),
      };
      const runtime = makeMcpRuntime({
        db: f.db,
        deploymentId: f.deploymentId,
        enabled: () => enabled,
        configuration: {
          infrastructure,
          control: tls,
          registrations: [],
          workers: new Map([["public", { enabled: true, deploymentId: f.deploymentId, tls }]]),
        },
        bridge: (client) => client,
      });
      if (!runtime.capabilities || !runtime.lifecycle) throw Error("configured runtime missing");
      const capabilitySpy = spyOn(capabilities, "mcpSessionCapabilities").mockReturnValue(
        runtime.capabilities,
      );
      try {
        const deps: CreateTaskDeps = {
          db: f.db,
          profiles: makeProfileStore(f.db),
          connections: f.connections,
          images: {
            listEnabledImages: async () => ({ images: [{ id: "image", imageUri: "image" }] }),
          },
          connectors: { list: async () => [] },
          harnessCatalog: {
            listHarnesses: async () => ({
              harnesses: [
                {
                  name: "claude",
                  descriptor: { auth: { userEnv: "TEST_TOKEN" }, models: [], effort: [] },
                },
              ],
            }),
          },
          secrets: { get: async () => "neutral-token", getAll: async () => ({}) },
          oauth: { listCredentials: async () => ({ credentials: [] }) },
          orgSecret: { listSecrets: async () => ({ secrets: [] }) },
          sessions: {
            createSession: async () => {
              throw Error("generic creation must not run");
            },
            deleteSession: async (req) => {
              deleted.push(req.sessionId);
            },
          },
          newTaskId: () => taskId,
          newSessionId: () => sessionId,
        };
        const create = () =>
          existing
            ? createSessionForExistingTask(deps, {
                taskId,
                profileId: f.profileId,
                ownerUserId: f.actorId,
                role: "primary",
                registerListener: true,
              })
            : createTaskWithSession(deps, {
                type: "chat",
                profileId: f.profileId,
                ownerUserId: f.actorId,
                prompt: "Neutral task",
              });
        if (lostReply) {
          await expect(create()).rejects.toBeInstanceOf(McpManagedCreationPending);
          expect(
            await f.db
              .select()
              .from(sessionListener)
              .where(eq(sessionListener.sessionId, sessionId)),
          ).toHaveLength(0);
          enabled = false;
          await runtime.lifecycle.runOnce(sessionId);
          await runtime.lifecycle.runOnce(sessionId);
        } else {
          expect((await create()).sessionId).toBe(sessionId);
          // Re-enrollment must not reset a listener lease or terminal marker.
          await f.db
            .update(sessionListener)
            .set({ owner: "listener-owner", terminalAt: f.now })
            .where(eq(sessionListener.sessionId, sessionId));
          const [listener] = await f.db
            .select()
            .from(sessionListener)
            .where(eq(sessionListener.sessionId, sessionId));
          await registerSessionListener(f.db, sessionId);
          expect(
            (
              await f.db
                .select()
                .from(sessionListener)
                .where(eq(sessionListener.sessionId, sessionId))
            )[0],
          ).toEqual(listener);
        }
        expect(reserves).toBe(1);
        expect(deleted).toEqual([]);
        expect(await f.db.select().from(task).where(eq(task.id, taskId))).toHaveLength(1);
        const rows = await f.db
          .select()
          .from(taskSession)
          .where(eq(taskSession.sessionId, sessionId));
        expect(rows).toHaveLength(1);
        expect(rows[0]).toMatchObject({
          taskId,
          integrationPrincipalId: f.actorId,
          mcpManagedLifecycle: {
            phase: lostReply ? "reserving" : "held",
            identity: { sessionId, taskId },
          },
        });
        expect(
          await f.db.select().from(sessionListener).where(eq(sessionListener.sessionId, sessionId)),
        ).toHaveLength(1);
      } finally {
        capabilitySpy.mockRestore();
        controlSpy.mockRestore();
        await f.db.delete(sessionListener).where(eq(sessionListener.sessionId, sessionId));
        await f.db.delete(taskSession).where(eq(taskSession.sessionId, sessionId));
        if (!existing) await f.db.delete(task).where(eq(task.id, taskId));
        await f.cleanup();
      }
    });
  }
}
