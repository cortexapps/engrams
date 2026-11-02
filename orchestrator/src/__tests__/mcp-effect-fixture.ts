import { operatorPublication } from "./mcp-binding-fixture.ts";
import { expect } from "bun:test";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { once } from "node:events";
import { create, toJson } from "@bufbuild/protobuf";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import { eq } from "drizzle-orm";
import { workerTls } from "../../../mcp-worker/test/fixtures/worker-tls.ts";
import type { WorkerIdentity } from "../../../mcp-worker/src/contracts.ts";
import { reviewFixture } from "./mcp-review-fixture.ts";
import { makeProfileStore } from "../db/profiles.ts";
import { user, taskSession, integrationConnection } from "../db/schema.ts";
import { compileSessionBindings } from "../mcp/session-bindings.ts";
import {
  bindingSnapshotColumns,
  readLiveBindingAuthority,
  makeStoredSessionBindingResolver,
} from "../mcp/binding-store.ts";
import {
  CredentialControlService,
  CredentialKind,
  CredentialStatusSchema,
} from "../gen/engram/mcp/v1/control_pb.ts";
import { SessionPolicyService } from "../gen/engram/mcp/v1/session_policy_pb.ts";
import {
  McpWritePolicyRefSchema,
  type McpWritePolicyRef,
} from "../gen/engram/app/v1/session_pb.ts";
import { statusMetadata } from "../mcp/credential-store.ts";
import { unwrapCredentialControlClient } from "../mcp/control-client.ts";
import { makeMcpEffectDriver } from "../mcp/effect-driver.ts";

export async function effectFixture(
  options: {
    classification?: "read" | "write";
    response?: string;
    constraints?: Record<string, unknown>[];
    outputSchema?: Record<string, unknown>;
    eligibility?: string;
    published?: boolean;
    bindingEpoch?: number;
  } = {},
) {
  const tls = await workerTls("localhost");
  const child = spawn(
    process.env.MCP_WORKER_TEST_NODE ?? "node",
    [new URL("../../../mcp-worker/test/fixtures/effect-worker.ts", import.meta.url).pathname],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { PATH: process.env.PATH ?? "", TMPDIR: process.env.TMPDIR ?? "/tmp" },
    },
  );
  const output = createInterface({ input: child.stdout }),
    lines = output[Symbol.asyncIterator]();
  let stderr = "";
  child.stderr.on("data", (data) => {
    stderr += String(data);
  });
  child.stdin.write(`${JSON.stringify({ tls, response: options.response })}\n`);
  async function next() {
    const line = await lines.next();
    if (line.done) throw Error(`fixture exited: ${stderr}`);
    return JSON.parse(line.value);
  }
  const ready: { port: number; identity: WorkerIdentity; node: string } = await next();
  expect(ready.node.startsWith("24.")).toBe(true);
  const f = await reviewFixture(options.constraints, options.classification === "write" ? 2 : 1);
  try {
    const classification = options.classification ?? "read";
    const tools = f.authority.profileGrants.map((grant) => ({
      upstreamName: grant.mcp!.upstreamName,
      description: "Fixture operation",
      inputSchema: { type: "object", properties: { x: { type: "integer" } }, required: ["x"] },
      ...(options.outputSchema ? { outputSchema: options.outputSchema } : {}),
    }));
    const current = (await f.connections.get(f.connection.id))!;
    const draft = await f.repository.saveDraft(
      current.id,
      current.mcp!,
      tools,
      f.actorId,
      new Date(),
    );
    await f.repository.publish(
      current.id,
      draft.id,
      1,
      tools.map((tool, index) => ({
        ...tool,
        classification: index === 0 ? classification : "read",
        enabled: true,
      })),
      f.actorId,
      new Date(),
    );
    await f.repository.setEnabled(current.id, 1, true, new Date());
    const profiles = makeProfileStore(f.db);
    await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
    if (options.published !== false)
      await profiles.update(f.profileId, (await profiles.get(f.profileId))!, {
        actorId: f.actorId,
        expectedRevision: 0,
      });
    await f.db.update(user).set({ role: "user" }).where(eq(user.id, f.actorId));
    const operatorPolicies = [
      {
        publication: structuredClone(operatorPublication),
        zoneId: ready.identity.zoneId,
        policyRevision: ready.identity.policyRevision,
        policyDigest: `sha256:${ready.identity.policyDigest}`,
        authorities: ["tools.example.test"],
        aliases: [],
      },
    ];
    const authority = await readLiveBindingAuthority(f.profileId, operatorPolicies, f.db);
    const snapshot = compileSessionBindings({
      launchPolicy: { profileId: f.profileId, integrationGrants: authority.profileGrants },
      liveAuthority: authority,
      bindingEpoch: options.bindingEpoch ?? 0,
    });
    await f.db
      .update(taskSession)
      .set(
        bindingSnapshotColumns(snapshot, [
          {
            id: current.id,
            alias: current.alias,
            provider: "mcp",
            displayName: current.displayName,
            config: current.config,
          },
        ]),
      )
      .where(eq(taskSession.sessionId, f.sessionId));
    const binding = snapshot.immutableBindings[0]!;
    const request = {
      sessionId: f.sessionId,
      runId: "run-1",
      toolCallId: "call-1",
      modelToolName: binding.tools.find((bound) => bound.tool.upstreamName === "Write.Record")!
        .name,
      originalArguments: '{ "x": 1 }',
    };
    const requests = new Map([[request.toolCallId, request]]);
    const events: string[] = [];
    let eligibility = options.eligibility ?? "write_eligible",
      fenceCurrent = true,
      token = "known-literal-token",
      leases = 0;
    let enabled = true;
    let authorityFailures = 0;
    let clockOffsetMs = 0;
    let beforeFence: (() => Promise<void>) | undefined;
    let beforeLease: (() => Promise<void>) | undefined;
    let afterLease: (() => Promise<void>) | undefined;
    const policies = new Map<
      string,
      { ref: McpWritePolicyRef; ancestorPolicyRefs: McpWritePolicyRef[] }
    >([
      [
        f.sessionId,
        {
          ref: create(McpWritePolicyRefSchema, {
            ...f.input.requestPolicyContext.acceptedPolicyRef,
            bindingEpoch: BigInt(snapshot.bindingEpoch),
          }),
          ancestorPolicyRefs: f.input.requestPolicyContext.ancestorPolicyRefs,
        },
      ],
    ]);
    const policy = createClient(
      SessionPolicyService,
      createRouterTransport((router) =>
        router.service(SessionPolicyService, {
          async readWritePolicy(req) {
            const refs = policies.get(req.sessionId)!;
            return {
              state: {
                policy: {
                  current: true,
                  initialized: eligibility !== "unknown",
                  writeEligibility: eligibility,
                  ...refs,
                },
              },
            };
          },
          async validateExecutionIdentity(req) {
            events.push("read-fence");
            await beforeFence?.();
            const original = requests.get(req.toolCallId);
            return {
              protocol: 1,
              status:
                fenceCurrent &&
                original?.originalArguments === req.originalArguments &&
                original?.modelToolName === req.modelToolName
                  ? "current"
                  : "stale",
            };
          },
          async validateDispatchFence(req) {
            events.push("write-fence");
            await beforeFence?.();
            const original = requests.get(req.toolCallId);
            return {
              protocol: 1,
              status:
                fenceCurrent &&
                eligibility === "write_eligible" &&
                original?.originalArguments === req.originalArguments &&
                original?.modelToolName === req.modelToolName
                  ? "current"
                  : "stale",
            };
          },
        }),
      ),
    );
    const status = create(CredentialStatusSchema, {
      protocol: 1,
      status: "connected",
      version: 1n,
      context: {
        connectionId: current.id,
        configRevision: BigInt(binding.configRevision),
        authorizationGeneration: BigInt(binding.authGeneration),
        credentialIdentity: binding.credentialIdentity,
        resource: { endpoint: binding.config.endpoint, zoneId: binding.config.zone },
        policyDigest: ready.identity.policyDigest,
        policyRevision: ready.identity.policyRevision,
      },
      reference: {
        kind: CredentialKind.API_KEY,
        connectionId: current.id,
        name: "mcp.test.key",
        generation: 1n,
      },
    });
    await f.db
      .update(integrationConnection)
      .set({ mcpCredential: { status: statusMetadata(status) } })
      .where(eq(integrationConnection.id, current.id));
    const control = unwrapCredentialControlClient(
      createClient(
        CredentialControlService,
        createRouterTransport((router) =>
          router.service(CredentialControlService, {
            async lease(req) {
              events.push("lease");
              await beforeLease?.();
              leases++;
              await afterLease?.();
              return {
                protocol: 1,
                intentId: req.intentId,
                expiresAt: req.expiresAt,
                operationExpiresAt: req.operationExpiresAt,
                header: "authorization",
                value: `Bearer ${token}`,
              };
            },
          }),
        ),
      ),
    );
    // Operator policy, host enforcement ACK, and coordinator history are fixture trusted boundaries.
    const resolver = makeStoredSessionBindingResolver(
      {
        control: policy,
        readOperatorPolicies: async () => {
          if (authorityFailures > 0) {
            authorityFailures--;
            throw Error("private-authority-transport-error");
          }
          return operatorPolicies;
        },
        enforceManagedDeny: async (expected) => expected,
      },
      f.db,
    );
    const driver = makeMcpEffectDriver({
      db: f.db,
      deploymentId: f.deploymentId,
      control,
      policy,
      resolver,
      workerTls: () => ({
        ...tls,
        url: `https://127.0.0.1:${ready.port}`,
        serverFingerprint: tls.fingerprint,
      }),
      enabled: () => enabled,
      now: () => new Date(Date.now() + clockOffsetMs),
    });
    return {
      ...f,
      driver,
      control,
      remote: {
        workerTls: {
          ...tls,
          url: `https://127.0.0.1:${ready.port}`,
          serverFingerprint: tls.fingerprint,
        },
        operatorPolicies,
        status: toJson(CredentialStatusSchema, status),
        eligibility,
        policyRef: toJson(McpWritePolicyRefSchema, f.input.requestPolicyContext.acceptedPolicyRef),
      },
      resolver,
      request,
      requests,
      snapshot,
      status,
      events,
      policies,
      operatorPolicies,
      advanceNow(milliseconds: number) {
        clockOffsetMs += milliseconds;
      },
      failAuthorityReads(count: number) {
        authorityFailures = count;
      },
      setEnabled(value: boolean) {
        enabled = value;
      },
      beforeFence(fn: () => Promise<void>) {
        beforeFence = fn;
      },
      get leases() {
        return leases;
      },
      setEligibility(value: string) {
        eligibility = value;
      },
      setFence(value: boolean) {
        fenceCurrent = value;
      },
      beforeLease(fn: () => Promise<void>) {
        beforeLease = fn;
      },
      afterLease(fn: () => Promise<void>) {
        afterLease = fn;
      },
      refresh() {
        token = "refreshed-literal-token";
      },
      async command(command: string) {
        child.stdin.write(`${command}\n`);
        return next();
      },
      async cleanup() {
        try {
          await f.cleanup();
        } finally {
          const exited = once(child, "exit");
          const timeout = setTimeout(() => child.kill("SIGKILL"), 5000);
          child.stdin.end("close\n");
          try {
            await exited;
          } finally {
            clearTimeout(timeout);
            output.close();
          }
          expect(stderr).toBe("");
        }
      },
    };
  } catch (error) {
    try {
      await f.cleanup();
    } finally {
      if (child.exitCode === null && child.signalCode === null) {
        const exited = once(child, "exit");
        child.kill("SIGKILL");
        await exited;
      }
      output.close();
    }
    throw error;
  }
}
