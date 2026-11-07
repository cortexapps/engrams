import { afterAll, beforeAll, expect, spyOn, test } from "bun:test";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { createServer } from "node:https";
import { X509Certificate } from "node:crypto";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import { Code, ConnectError } from "@connectrpc/connect";
import {
  ContextSchema,
  realOwnedPath,
  sha256,
  RunnerFailure,
} from "./context.ts";
import { bindingTools, modelToolName } from "./bootstrap.ts";
import { DatabaseObserver } from "./observer.ts";
import { publicClient } from "./transport.ts";
import { IntegrationService } from "../../src/gen/engram/app/v1/integration_pb.ts";
import { ProfileService } from "../../src/gen/engram/app/v1/profile_pb.ts";
import { TaskService } from "../../src/gen/engram/app/v1/task_pb.ts";
import { SessionService } from "../../src/gen/engram/app/v1/session_pb.ts";
import {
  AutomationService,
  AutomationRunService,
} from "../../src/gen/engram/app/v1/automation_pb.ts";
import { createAutomationTask } from "./automation.ts";
import { readOwnedAutomationRuns } from "./automation-ownership.ts";

let root: string;
let cert: Buffer;
let key: Buffer;
beforeAll(async () => {
  const actual = join(import.meta.dir, "../../..", "var/mcp-stack-runner");
  await realOwnedPath(actual, actual);
  root = await mkdtemp(join(actual, "infrastructure-unit-"));
  const keyPath = join(root, "https.key"),
    certPath = join(root, "https.pem");
  const process = Bun.spawn(
    [
      "openssl",
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-keyout",
      keyPath,
      "-out",
      certPath,
      "-days",
      "1",
      "-subj",
      "/CN=localhost",
      "-addext",
      "subjectAltName=DNS:localhost",
    ],
    { stdout: "ignore", stderr: "ignore" },
  );
  expect(await process.exited).toBe(0);
  await chmod(certPath, 0o600);
  cert = await readFile(certPath);
  key = await readFile(keyPath);
});
afterAll(async () => {
  if (root) await rm(root, { recursive: true });
});
const h = "a".repeat(64);
const artifact = () => ({ path: "/owned/a", sha256: h });
const ep = (name: string) => ({
  origin: `https://${name}.fixture.example`,
  ca: artifact(),
  serverFingerprint: Array(32).fill("AA").join(":"),
});
function validContext() {
  return {
    version: 1,
    runId: "unit",
    runRoot: "/owned",
    backend: "vz",
    sourceCommit: "a".repeat(40),
    hostId: "11111111-1111-4111-8111-111111111111",
    imageId: "fixture-image",
    imageDigest: `sha256:${h}`,
    artifacts: Object.fromEntries(
      [
        "coordinator",
        "orchestrator",
        "worker",
        "hostAgent",
        "fixture",
        "kernel",
        "guestImage",
        "claudeHarness",
        "codexHarness",
        "modelCli",
        "claudeBundle",
        "codexBundle",
        "helper",
      ].map((key) => [key, artifact()]),
    ),
    endpoints: {
      app: ep("app"),
      coordinator: ep("coordinator"),
      workerControl: ep("worker-control"),
      operator: {
        ...ep("operator"),
        certificate: artifact(),
        privateKeyFile: "/owned/key",
      },
    },
    protocolFixtures: {
      modern: {
        ...ep("observer"),
        configuration: artifact(),
        runRoot: "/owned/modern",
        resourceKeyFile: "/owned/modern/resource.key",
        tokenFile: "/owned/token",
        resourceOrigin: ep("resource").origin,
        issuerOrigin: ep("issuer").origin,
        authorizationOrigin: ep("authorization").origin,
        tokenOrigin: ep("token").origin,
        redirectUri: ep("app").origin + "/integrations/mcp/callback",
        protocol: "2026-07-28",
      },
      legacy: {
        ...ep("observer-legacy"),
        configuration: artifact(),
        runRoot: "/owned/legacy",
        resourceKeyFile: "/owned/legacy/resource.key",
        tokenFile: "/owned/legacy/token",
        resourceOrigin: ep("resource-legacy").origin,
        issuerOrigin: ep("issuer-legacy").origin,
        authorizationOrigin: ep("authorization-legacy").origin,
        tokenOrigin: ep("token-legacy").origin,
        redirectUri: ep("app").origin + "/integrations/mcp/callback",
        protocol: "2025-11-25",
      },
    },
    publication: {
      candidateIdentity: `sha256:${h}`,
      expectedGeneration: null,
      deploymentIdentity: `sha256:${h}`,
      networkIdentity: `sha256:${h}`,
      trustIdentity: `sha256:${h}`,
      zone: "fixture",
    },
    identities: {
      ...Object.fromEntries(
        ["admin", "owner", "other"].map((role) => [
          role,
          {
            email: `${role}@fixture.example`,
            passwordFile: `/owned/${role}.password`,
            cookieFile: `/owned/${role}.cookie`,
          },
        ]),
      ),
      unattendedKeyFile: "/owned/unattended.key",
      automationKeyFile: "/owned/automation.key",
      routerSentinelFile: "/owned/router.key",
    },
    supervisor: {
      socket: "/owned/control.sock",
      tokenFile: "/owned/supervisor.token",
      configurations: Object.fromEntries(
        ["orchestrator", "worker", "coordinator", "host-agent"].map((role) => [
          role,
          { normal: artifact() },
        ]),
      ),
      operatorCandidates: {
        normal: { file: artifact(), identity: `sha256:${h}` },
      },
    },
    observer: {
      coordinatorDsnFile: "/owned/c.dsn",
      hostManagedRoot: "/owned/host/managed-boots",
      hostLogFile: "/owned/host.log",
      orchestratorDsnFile: "/owned/o.dsn",
    },
    guest: {
      stateDirectory: "/tmp/mcp-stack-fixture",
      cliPath: "/opt/fixture/engram-mcp-model-fixture",
    },
    deadlines: { requestMs: 1000, stepMs: 3000, totalMs: 10000 },
    evidenceFile: "/owned/evidence.jsonl",
  };
}

for (const failure of [
  "none",
  "upload",
  "download",
  "update",
  "binding",
  "counter",
  "cleanup",
  "managed-upload",
  "managed-download",
  "upload-cleanup",
  "publication",
  "owner",
  "profile",
  "scenario-ordinary",
  "scenario-managed",
  "scenario-owner",
  "scenario-receipt",
  "scenario-tools",
  "scenario-executions",
  "scenario-policy",
  "scenario-pagination",
  "scenario-malformed",
] as const)
  test(`infrastructure uses public owner tasks, managed authority and exact cleanup: ${failure}`, async () => {
    const scenarioTest = failure.startsWith("scenario-");
    const ordinaryScenario = scenarioTest && failure !== "scenario-managed";
    const managedIndex = scenarioTest ? 0 : 1;
    const ctx = ContextSchema.parse(validContext());
    ctx.runRoot = root;
    ctx.protocolFixtures.modern.resourceKeyFile = join(root, "resource.key");
    ctx.protocolFixtures.modern.tokenFile = join(root, "observer.token");
    ctx.observer.coordinatorDsnFile = join(root, "coordinator.dsn");
    ctx.observer.orchestratorDsnFile = join(root, "orchestrator.dsn");
    await writeFile(
      ctx.protocolFixtures.modern.resourceKeyFile,
      "fixture-neutral-resource-key",
      { mode: 0o600 },
    );
    await writeFile(ctx.protocolFixtures.modern.tokenFile, "t".repeat(32), {
      mode: 0o600,
    });
    for (const path of [
      ctx.observer.coordinatorDsnFile,
      ctx.observer.orchestratorDsnFile,
    ])
      await writeFile(path, "postgres://unit@unused.fixture.example/mcp_unit", {
        mode: 0o600,
      });
    // Pools remain disconnected. Only the managed read-only observations use test values.
    const database = await DatabaseObserver.connect(ctx);
    const events: string[] = [];
    const sessionIds = [
      "11111111-1111-4111-8111-111111111112",
      "11111111-1111-4111-8111-111111111113",
    ];
    const source_key = {
      creation: {
        session_id: sessionIds[managedIndex]!,
        task_id: scenarioTest ? "task-1" : "task-2",
        policy: {
          actorId: failure === "owner" ? "other" : "owner",
          inputId: "input",
          parentSessionId: null,
          eligibility: "interactive",
        },
        input_digest: `sha256:${h}`,
      },
      context: {
        target: {
          session_id: sessionIds[managedIndex]!,
          sandbox_id: sessionIds[managedIndex]!,
          host_id: ctx.hostId,
          binding_epoch: 1,
        },
        operator_generation: 1,
        operator_identity:
          failure === "publication"
            ? `sha256:${"b".repeat(64)}`
            : ctx.publication.candidateIdentity,
        binding_hash: null,
      },
      boot_attempt: "boot",
      native_digest: `sha256:${h}`,
    };
    const started = spyOn(database, "started").mockImplementation(
      async (sessionId, hostId) => {
        if (ordinaryScenario)
          throw new RunnerFailure(
            "ASSERTION_FAILED",
            "coordinator_managed_boot_row",
          );
        expect(sessionId).toBe(sessionIds[managedIndex]!);
        expect(hostId).toBe(ctx.hostId);
        events.push("managed-started");
        return {
          phase: "started",
          host_id: hostId,
          binding_epoch: "1",
          boot_attempt: "boot",
          native_digest: `sha256:${h}`,
          source_key,
          combined_manifest_digest: `sha256:${h}`,
          binding_hash: `sha256:${h}`,
        };
      },
    );
    const lifecycle = spyOn(database, "managedLifecycle").mockImplementation(
      async (sessionId) => {
        expect(sessionId).toBe(sessionIds[managedIndex]!);
        events.push("managed-lifecycle");
        return {
          phase: "started",
          nativeDigest: `sha256:${h}`,
          bindingHash: `sha256:${h}`,
        };
      },
    );
    const connection = {
      id: "connection",
      alias: "fixture-infrastructure-managed",
      provider: "mcp",
      enabled: true,
      configRevision: 7,
      authGeneration: 6,
      mcp: {
        endpoint: ctx.protocolFixtures.modern.resourceOrigin + "/mcp",
        zone: "fixture",
        authKind: "api_key",
      },
    };
    const tools = ["read_counter", "write_counter"].map((upstreamName) => ({
      upstreamName,
      inputSchemaJson: '{"type":"object"}',
    }));
    const alias = modelToolName(
      connection.id,
      connection.alias,
      "read_counter",
    );
    let profiles = 0,
      tasks = 0,
      observations = 0;
    const deletedTasks: string[] = [],
      deletedProfiles: string[] = [];
    const bytes = Buffer.alloc(4096, 65);
    const handler = connectNodeAdapter({
      requestPathPrefix: "/rpc",
      routes(router) {
        router.service(ProfileService, {
          createProfile(req, rpc) {
            expect(rpc.requestHeader.get("cookie")).toBe(
              "fixture_session=admin",
            );
            profiles++;
            events.push(`profile-${profiles}`);
            expect(req.imageId).toBe(ctx.imageId);
            expect(req.harness).toBe("claude");
            expect(req.modelRouter).toBe("openrouter");
            expect(req.model).toBe("z-ai/glm-5.2");
            if (profiles === 1 && failure !== "scenario-managed")
              expect(req.integrationGrants).toEqual([]);
            else {
              expect(req.integrationGrants).toHaveLength(1);
              expect(req.integrationGrants[0]).toMatchObject({
                connectionId: connection.id,
                mcp: {
                  upstreamName: "read_counter",
                  automationWrite: false,
                  unrestricted: true,
                },
              });
              expect(req.network?.default).toBe("deny");
            }
            return {
              profile: {
                id: `profile-${profiles}`,
                harness: req.harness,
                modelRouter: req.modelRouter,
                model: req.model,
                mcpGrantRevision: failure === "profile" ? 0n : 1n,
              },
            };
          },
          deleteProfile(req) {
            deletedProfiles.push(req.id);
            events.push(`delete-${req.id}`);
            return {};
          },
        });
        router.service(TaskService, {
          createTask(req, rpc) {
            expect(rpc.requestHeader.get("cookie")).toBe(
              "fixture_session=owner",
            );
            expect(req.deferInitialPrompt).toBe(true);
            expect(req.prompt).toBeUndefined();
            expect(req.type).toBe("chat");
            tasks++;
            events.push(`task-${tasks}`);
            return {
              task: {
                id: `task-${tasks}`,
                sessions: [
                  { sessionId: sessionIds[tasks - 1]!, isPrimary: true },
                ],
              },
            };
          },
          updateTask(req) {
            if (failure === "update")
              throw new ConnectError("private", Code.FailedPrecondition);
            return { task: { id: req.taskId, titleIsCustom: true } };
          },
          getTask(req) {
            return {
              task: {
                id: req.taskId,
                createdByUserId:
                  failure === "scenario-owner" ? "other" : "owner",
                sessions: [{ sessionId: sessionIds[0]! }],
              },
            };
          },
          deleteTask(req) {
            deletedTasks.push(req.taskId);
            events.push(`delete-${req.taskId}`);
            if (failure === "cleanup" || failure === "upload-cleanup")
              throw new ConnectError("private", Code.Unavailable);
            return {};
          },
        });
        router.service(SessionService, {
          getSession(req) {
            return {
              session: {
                id: req.sessionId,
                status: "active",
                hostId: ctx.hostId,
              },
            };
          },
          async *exec(req, rpc) {
            expect(rpc.requestHeader.get("cookie")).toBe(
              "fixture_session=owner",
            );
            events.push(`exec-${tasks}`);
            expect(req.argv[0]).toBe("/bin/sh");
            const script = req.argv[2]!;
            yield {
              event: { case: "started" as const, value: { execId: "fixed" } },
            };
            if (script.includes("infrastructure-exec-ok")) {
              expect(script).toContain("umask 077");
              expect(script).toContain("set -C");
              expect(script).toContain("stat -c %u");
              expect(script).toContain("stat -c %h");
              expect(script).toContain("stat -c %a");
              expect(script).toContain("sha256sum");
              expect(req.argv[4]).toBe(ctx.guest.stateDirectory);
              yield {
                event: {
                  case: "stdout" as const,
                  value: Buffer.from("infrastructure-exec-ok\n"),
                },
              };
            }
            yield {
              event: { case: "exit" as const, value: { exitStatus: 0 } },
            };
          },
        });
        router.service(IntegrationService, {
          createConnection() {
            events.push("catalog");
            return { connection };
          },
          putMcpCredential() {
            return { connection };
          },
          discoverMcp() {
            return { catalog: { id: "draft", kind: "draft", tools } };
          },
          getMcpCatalogs() {
            return { draft: { id: "draft" }, catalogRevision: 1 };
          },
          publishMcp() {
            return { catalog: { id: "published", kind: "published", tools } };
          },
          setConnectionEnabled() {
            return { connection };
          },
          getSessionMcpState(req) {
            if (ordinaryScenario) {
              expect(req.sessionId).toBe(sessionIds[0]!);
              expect(req.omitBindings).toBe(false);
              return {
                availability:
                  failure === "scenario-malformed" ? "" : "unavailable",
                ...(failure === "scenario-receipt"
                  ? { bindingReceipt: { current: false } }
                  : {}),
                ...(failure === "scenario-policy" ? { writePolicy: {} } : {}),
                ...(failure === "scenario-tools"
                  ? { bindingTools: [{ modelToolName: alias }] }
                  : {}),
                ...(failure === "scenario-executions"
                  ? { executions: [{ executionId: "unexpected" }] }
                  : {}),
                ...(failure === "scenario-pagination"
                  ? { hasMoreBindingTools: true }
                  : {}),
              };
            }
            expect(req.sessionId).toBe(sessionIds[managedIndex]!);
            events.push("binding");
            return {
              bindingReceipt: { current: true },
              bindingTools:
                failure === "binding"
                  ? []
                  : [
                      {
                        connectionId: connection.id,
                        modelToolName: alias,
                        reviewedTool: tools[0],
                      },
                    ],
            };
          },
        });
      },
    });
    const server = createServer({ key, cert }, (req, res) => {
      if (req.url === "/v1/observe") {
        observations++;
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            version: 1,
            healthy: true,
            effects: { version: 1, executions: 0, count: 0, events: [] },
            reads: failure === "counter" && observations > 1 ? 1 : 0,
            held: 0,
            cancellations: 0,
            oauth: {
              account: "account-one",
              scope: null,
              authorizations: 0,
              exchanges: 0,
              rotations: 0,
              rejections: 0,
              expiresInSeconds: 600,
            },
          }),
        );
        return;
      }
      if (req.url?.startsWith("/api/v1/sessions/")) {
        const owner = req.headers.cookie === "fixture_session=owner";
        if (!owner) {
          res.writeHead(req.headers.cookie ? 404 : 401);
          res.end("private");
          return;
        }
        expect(req.headers.origin).toBe(ctx.endpoints.app.origin);
        if (req.method === "POST") {
          events.push(`upload-${tasks}`);
          const chunks: Buffer[] = [];
          req.on("data", (chunk: Buffer) => chunks.push(chunk));
          req.on("end", () => {
            expect(Buffer.concat(chunks)).toEqual(bytes);
            if (
              failure === "upload" ||
              failure === "upload-cleanup" ||
              (failure === "managed-upload" && tasks === 2)
            ) {
              res.writeHead(400, { "x-engram-rpc-code": "3" });
              res.end("private");
              return;
            }
            res.writeHead(200, { "content-type": "application/json" });
            res.end(
              JSON.stringify({
                path: new URL(
                  req.url!,
                  ctx.endpoints.app.origin,
                ).searchParams.get("path"),
                size_bytes: 4096,
                sha256: sha256(bytes),
              }),
            );
          });
          return;
        }
        events.push(`download-${tasks}`);
        res.writeHead(
          failure === "download" ||
            (failure === "managed-download" && tasks === 2)
            ? 404
            : 200,
          { "content-length": "4096", "x-content-sha256": sha256(bytes) },
        );
        res.end(bytes);
        return;
      }
      void handler(req, res);
    });
    await new Promise<void>((resolve) =>
      server.listen(0, "localhost", resolve),
    );
    try {
      const address = server.address();
      if (!address || typeof address === "string") throw new Error("address");
      const endpoint = {
        origin: `https://localhost:${address.port}`,
        ca: { path: join(root, "https.pem"), sha256: sha256(cert) },
        serverFingerprint: new X509Certificate(cert).fingerprint256,
      };
      ctx.endpoints.app = endpoint;
      Object.assign(ctx.protocolFixtures.modern, endpoint);
      const admin = await publicClient(ctx, {
        kind: "cookie",
        value: "fixture_session=admin",
      });
      const owner = await publicClient(ctx, {
        kind: "cookie",
        value: "fixture_session=owner",
      });
      const other = await publicClient(ctx, {
        kind: "cookie",
        value: "fixture_session=other",
      });
      const principals = {
        admin,
        owner,
        other,
        unattended: other,
        automation: other,
        actorIds: {
          admin: "admin",
          owner: "owner",
          other: "other",
          unattended: "unattended",
          automation: "automation",
        },
      };
      const { runInfrastructure } = await import("./infrastructure.ts");
      if (scenarioTest) {
        const { startScenario, scenarioBindingTools } = await import(
          "./scenario-runtime.ts"
        );
        const pending = startScenario(ctx, principals, database, {
          harness: "claude",
          suffix: "claude-grant-missing",
          protocol: "modern",
          upstreamName: "read_counter",
          mode: "read",
          caller: "owner",
          automationWrite: false,
          arguments: {},
          constraints: [],
          callId: "grant-missing",
          ...(ordinaryScenario ? { grant: false } : {}),
        });
        if (failure === "scenario-ordinary" || failure === "scenario-managed") {
          const scenario = await pending;
          expect(scenario.sessionId).toBe(sessionIds[0]!);
          const observed = await scenarioBindingTools(scenario);
          if (ordinaryScenario) {
            expect(observed).toEqual([]);
            expect(() => scenario.coordinator).toThrow(
              "managed_scenario_required",
            );
            await expect(
              bindingTools(owner, scenario.sessionId),
            ).rejects.toThrow("current_binding");
          } else {
            expect(observed.map((tool) => tool.modelToolName)).toEqual([alias]);
            expect(scenario.coordinator.phase).toBe("started");
          }
        } else {
          await expect(pending).rejects.toThrow(
            failure === "scenario-owner"
              ? "ordinary_owner_task_session"
              : "ordinary_empty_mcp_authority",
          );
        }
        expect(started).toHaveBeenCalledTimes(ordinaryScenario ? 0 : 1);
        expect(lifecycle).toHaveBeenCalledTimes(ordinaryScenario ? 0 : 1);
        return;
      }
      if (failure === "none") {
        const rows = await runInfrastructure(ctx, principals, database);
        expect(rows).toEqual(
          (["ordinary", "managed"] as const).map((mode) => ({
            mode,
            execSha256: sha256("infrastructure-exec-ok\n"),
            uploadSha256: sha256(bytes),
            downloadSha256: sha256(bytes),
            sizeBytes: 4096,
            unauthenticatedStatus: 401,
            nonOwnerStatus: 404,
          })),
        );
        expect(events.indexOf("delete-task-1")).toBeLessThan(
          events.indexOf("catalog"),
        );
        expect(events.indexOf("managed-lifecycle")).toBeLessThan(
          events.indexOf("binding"),
        );
        expect(events.indexOf("binding")).toBeLessThan(
          events.indexOf("upload-2"),
        );
        expect(started).toHaveBeenCalledTimes(1);
        expect(lifecycle).toHaveBeenCalledTimes(1);
      } else {
        const seams: Partial<Record<typeof failure, string>> = {
          upload: "guest_file_status_http_400_rpc_code_3",
          download: "guest_file_status_http_404",
          update: "private",
          binding: "exact_public_binding",
          counter: "infrastructure_no_tool_execution",
          cleanup: "private",
          "managed-upload": "guest_file_status_http_400_rpc_code_3",
          "managed-download": "guest_file_status_http_404",
          "upload-cleanup": "guest_file_status_http_400_rpc_code_3",
          publication: "infrastructure_started_owner_publication",
          owner: "infrastructure_started_owner_publication",
          profile: "published_fixture_profile",
        };
        const seam = seams[failure];
        expect(seam).toBeDefined();
        await expect(
          runInfrastructure(ctx, principals, database),
        ).rejects.toThrow(seam);
        if (
          [
            "upload",
            "download",
            "update",
            "cleanup",
            "upload-cleanup",
          ].includes(failure)
        )
          expect(tasks).toBe(1);
      }
      expect(deletedTasks).toEqual(
        Array.from({ length: tasks }, (_, i) => `task-${i + 1}`),
      );
      expect(deletedProfiles).toEqual(
        Array.from({ length: profiles }, (_, i) => `profile-${i + 1}`),
      );
    } finally {
      started.mockRestore();
      lifecycle.mockRestore();
      await database.close();
      server.closeAllConnections();
      await new Promise<void>((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      );
    }
  });

for (const state of [
  "running",
  "pending-visible-launch",
  "completed",
  "filtered",
  "failed",
  "superseded",
  "halted",
  "deadline",
  "ended",
] as const)
  test(`automation fixture keeps live authority through the case: ${state}`, async () => {
    const ctx = ContextSchema.parse(validContext());
    ctx.runRoot = root;
    const automationId = `fixture-automation-${state}`;
    const runId = `autorun:${automationId}:manual:fixture`;
    const sessionId = "11111111-1111-4111-8111-111111111112";
    const taskId = "fixture-task";
    const events: string[] = [];
    let definitionJson: string | undefined;
    let observations = 0;
    const terminal = !["running", "pending-visible-launch"].includes(state);
    const handler = connectNodeAdapter({
      requestPathPrefix: "/rpc",
      routes(router) {
        router.service(AutomationService, {
          createAutomation(req, rpc) {
            expect(rpc.requestHeader.get("cookie")).toBe(
              "fixture_session=admin",
            );
            expect(req.name).toBe(`Fixture ${state}`);
            expect(req.enabled).toBe(true);
            expect(req.inputsJson).toBe("{}");
            events.push("create");
            definitionJson = req.definitionJson;
            return { automation: { id: automationId } };
          },
          runNow(req, rpc) {
            expect(rpc.requestHeader.get("x-api-key")).toBe(
              "engk_fixture_automation",
            );
            expect(req.automationId).toBe(automationId);
            events.push("run");
            return { runId };
          },
        });
        router.service(AutomationRunService, {
          async getRun(req, rpc) {
            expect(rpc.requestHeader.get("x-api-key")).toBe(
              "engk_fixture_automation",
            );
            expect(req.runId).toBe(runId);
            expect(
              (await readOwnedAutomationRuns(ctx)).find(
                (receipt) => receipt.runId === runId,
              ),
            ).toMatchObject({
              automationId,
              runId,
              definitionActorId: "definition-actor",
              profileId: "fixture-profile",
              fixtureRunId: ctx.runId,
              sourceCommit: ctx.sourceCommit,
              hostId: ctx.hostId,
            });
            events.push("observe");
            observations++;
            return {
              run: {
                brief: {
                  id: runId,
                  automationId,
                  status:
                    state === "pending-visible-launch"
                      ? observations === 1
                        ? "pending"
                        : "running"
                      : state === "ended"
                        ? "running"
                        : state,
                  triggerSource: "manual",
                  dryRun: false,
                  ...(state === "ended"
                    ? { endedAt: "2026-01-01T00:00:00Z" }
                    : {}),
                },
                sessionIds: [sessionId],
                steps: [
                  {
                    blockId: "launch",
                    status: "succeeded",
                    outputsJson: JSON.stringify({
                      session_id: sessionId,
                      task_id: taskId,
                      initial_prompt: false,
                      prompt: "",
                    }),
                  },
                ],
              },
            };
          },
        });
        router.service(TaskService, {
          async updateTask(req, rpc) {
            expect(rpc.requestHeader.get("cookie")).toBe(
              "fixture_session=admin",
            );
            expect(req.taskId).toBe(taskId);
            expect(req.title).toBe("Fixture automation task");
            expect(
              (await readOwnedAutomationRuns(ctx)).find(
                (receipt) => receipt.runId === runId,
              ),
            ).toMatchObject({ automationId, runId, sessionId, taskId });
            events.push("title");
            return { task: { id: taskId, titleIsCustom: true } };
          },
        });
        router.service(SessionService, {
          sendPrompt() {
            events.push("prompt");
            return {};
          },
        });
      },
    });
    const server = createServer({ cert, key }, handler);
    await new Promise<void>((resolve) =>
      server.listen(0, "localhost", resolve),
    );
    try {
      const address = server.address();
      if (!address || typeof address === "string") throw new Error("address");
      ctx.endpoints.app = {
        origin: `https://localhost:${address.port}`,
        ca: { path: join(root, "https.pem"), sha256: sha256(cert) },
        serverFingerprint: new X509Certificate(cert).fingerprint256,
      };
      const admin = await publicClient(ctx, {
        kind: "cookie",
        value: "fixture_session=admin",
      });
      const automation = await publicClient(ctx, {
        kind: "api-key",
        value: "engk_fixture_automation",
      });
      const principals = {
        admin,
        automation,
        owner: admin,
        other: admin,
        unattended: admin,
        actorIds: {
          admin: "definition-actor",
          automation: "trigger-actor",
          owner: "owner",
          other: "other",
          unattended: "unattended",
        },
      };
      const pending = createAutomationTask(
        ctx,
        principals,
        "fixture-profile",
        state,
      );
      if (terminal) {
        await expect(pending).rejects.toThrow("automation_launch_failed");
        expect(events).toEqual(["create", "run", "observe"]);
      } else {
        const result = await pending;
        expect(result).toMatchObject({
          taskId,
          sessionId,
          automation: {
            id: automationId,
            runId,
            runtimeActorId: `automation:${automationId}`,
            definitionActorId: "definition-actor",
            triggerActorId: "trigger-actor",
          },
        });
        expect(events).toEqual(
          state === "pending-visible-launch"
            ? ["create", "run", "observe", "observe", "title"]
            : ["create", "run", "observe", "title"],
        );
        expect(JSON.parse(definitionJson!)).toEqual({
          engine: 1,
          trigger: { kind: "manual" },
          blocks: [
            {
              id: "launch",
              type: "create_session",
              config: {
                profileId: "fixture-profile",
                promptTemplate: "",
                titleTemplate: "Fixture automation task",
                includeEventContext: false,
                keepOnFinish: true,
              },
            },
            {
              id: "lifetime",
              type: "wait_session",
              config: { session: { blockId: "launch" }, until: "ended" },
            },
          ],
          inputsSchema: [],
          settings: { endSessionsOnFinish: false, runDeadlineSeconds: 120 },
        });
      }
    } finally {
      server.closeAllConnections();
      await new Promise<void>((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      );
    }
  });
