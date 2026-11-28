import { assertSweepPoliciesExhaustive } from "../../sweep/policy.ts";
import { DBOS } from "@dbos-inc/dbos-sdk";
import { createInterface } from "node:readline";
import { Client } from "pg";
import { fromJson } from "@bufbuild/protobuf";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import { and, eq } from "drizzle-orm";
import { getDb } from "../../db/client.ts";
import { mcpExecution } from "../../db/schema.ts";
import { CredentialControlService } from "../../gen/engram/mcp/v1/control_pb.ts";
import { SessionPolicyService } from "../../gen/engram/mcp/v1/session_policy_pb.ts";
import { McpWritePolicyRefSchema } from "../../gen/engram/app/v1/session_pb.ts";
import { makeStoredSessionBindingResolver } from "../../mcp/binding-store.ts";
import { unwrapCredentialControlClient } from "../../mcp/control-client.ts";
import { makeMcpEffectDriver } from "../../mcp/effect-driver.ts";
import { makeMcpJournal } from "../../mcp/journal.ts";
import { makeMcpRecovery, installMcpRecovery } from "../../mcp/recovery.ts";
import { makeMcpRecoveryScanner } from "../../mcp/recovery-scanner.ts";
import { startMcpRequestWorkflow } from "../../workflows/mcp-exec.ts";
import { makeToolConsumer } from "../../listeners/tool-consumer.ts";
import { createToolRegistry } from "../../tools/registry.ts";
import { makePendingToolCallStore } from "../../tools/pending-tool-calls.ts";
import { verifyMcpTestDatabases } from "./mcp-test-isolation.ts";
import type { effectFixture } from "../mcp-effect-fixture.ts";

type Fixture = Awaited<ReturnType<typeof effectFixture>>;
interface Input {
  remote: Fixture["remote"];
  request: Fixture["request"];
  deploymentId: string;
  checkpoint?: string;
  workflowId: string;
  resume?: boolean;
  clockOffset?: number;
  eventIdx?: string;
  requestReadFailures?: number;
}
const lines = createInterface({ input: process.stdin });
const input: Input = JSON.parse((await lines[Symbol.asyncIterator]().next()).value!);
const { system } = await verifyMcpTestDatabases();
const now = () => new Date(Date.now() + (input.clockOffset ?? 0));
let releaseCheckpoint: (() => void) | undefined;
let postLeaseFences = 0;
let responseRead = false;
async function checkpoint(name: string) {
  if (name !== input.checkpoint) return;
  process.stdout.write(`CHECKPOINT ${name}\n`);
  await new Promise<void>((resolve) => {
    releaseCheckpoint = resolve;
  }); // Parent kills or releases the observed process.
}
// Test-only driver instrumentation observes real SQL before/after execution and commit.
// It never changes SQL, transaction results, authority, or the production effect path.
const transitions = new WeakMap<object, string>();
Client.prototype.query = new Proxy(Client.prototype.query, {
  apply(target, client, args) {
    const query = typeof args[0] === "string" ? args[0] : args[0]?.text;
    const values = args[1] ?? args[0]?.values ?? [];
    return (async () => {
      if (responseRead && query?.startsWith("select") && query.includes('from "mcp_execution"')) {
        responseRead = false;
        await checkpoint("response_before_persist");
      }
      if (query?.startsWith('update "mcp_admission"') && values.includes("execute"))
        await checkpoint("before_admission");
      if (query?.startsWith('update "mcp_approval" set "consumed_at"'))
        await checkpoint("approval_before_consume");
      if (query?.startsWith('insert into "mcp_execution"')) {
        await checkpoint("before_insert");
        transitions.set(client, "after_insert");
      }
      if (query?.startsWith('update "mcp_execution" set "state"')) {
        if (values[0] === "dispatching") {
          await checkpoint("intent_issued");
          transitions.set(client, "barrier_committed");
        }
        if (["succeeded", "failed"].includes(values[0])) {
          await checkpoint("response_verified");
          transitions.set(client, "result_committed");
        }
      }
      const result = await Reflect.apply(target, client, args);
      if (query?.startsWith('update "mcp_approval" set "consumed_at"'))
        await checkpoint("approval_consumed_uncommitted");
      if (query?.toLowerCase() === "commit") {
        const boundary = transitions.get(client);
        transitions.delete(client);
        if (boundary) await checkpoint(boundary);
      }
      if (query?.startsWith('update "mcp_execution" set "completion_enqueued_version"'))
        await checkpoint("receipt_committed");
      return result;
    })();
  },
});
const db = getDb(),
  journal = makeMcpJournal(db);
const ref = fromJson(McpWritePolicyRefSchema, input.remote.policyRef);
let leases = 0;
async function fence(req: {
  toolCallId: string;
  originalArguments: string;
  modelToolName: string;
}) {
  const [call] = await db
    .select()
    .from(mcpExecution)
    .where(
      and(
        eq(mcpExecution.sessionId, input.request.sessionId),
        eq(mcpExecution.toolCallId, input.request.toolCallId),
      ),
    );
  if (call) {
    const lease = await journal.getAdmission(call.id);
    if (call.state === "authorized" && lease?.phase === "execute" && lease.state === "admitted")
      await checkpoint("admission_committed");
    if (call.state === "dispatching" && leases > 0) await checkpoint("intent_leased");
  }
  if (leases > 0 && ++postLeaseFences === 2) responseRead = true;
  return {
    protocol: 1,
    status:
      req.toolCallId === input.request.toolCallId &&
      req.originalArguments === input.request.originalArguments &&
      req.modelToolName === input.request.modelToolName
        ? "current"
        : "stale",
  };
}
const policy = createClient(
  SessionPolicyService,
  createRouterTransport((router) =>
    router.service(SessionPolicyService, {
      readWritePolicy: async () => ({
        state: {
          policy: {
            current: true,
            initialized: input.remote.eligibility !== "unknown",
            writeEligibility: input.remote.eligibility,
            ref,
            ancestorPolicyRefs: [],
          },
        },
      }),
      validateExecutionIdentity: fence,
      validateDispatchFence: async (req) =>
        input.remote.eligibility === "write_eligible"
          ? fence(req)
          : { protocol: 1, status: "stale" },
    }),
  ),
);
const control = unwrapCredentialControlClient(
  createClient(
    CredentialControlService,
    createRouterTransport((router) =>
      router.service(CredentialControlService, {
        lease: async (req) => {
          leases++;
          await checkpoint("before_lease");
          return {
            protocol: 1,
            intentId: req.intentId,
            expiresAt: req.expiresAt,
            operationExpiresAt: req.operationExpiresAt,
            header: "authorization",
            value: "Bearer known-literal-token",
          };
        },
      }),
    ),
  ),
);
const resolver = makeStoredSessionBindingResolver(
  {
    control: policy,
    readOperatorPolicies: async () => input.remote.operatorPolicies,
    enforceManagedDeny: async (expected) => expected,
  },
  db,
);
const driver = makeMcpEffectDriver({
  db,
  deploymentId: input.deploymentId,
  control,
  policy,
  resolver,
  workerTls: () => input.remote.workerTls,
  enabled: () => true,
  now,
});
// The coordinator completion boundary is a parent-owned deterministic fixture. It stores a
// fixed-key outbox record before returning, and survives driver process death.
const completions: (() => void)[] = [];
lines.on("line", (line) => {
  if (line === "ACK") completions.shift()?.();
  if (line === "CONTINUE") releaseCheckpoint?.();
});
const runtime = makeMcpRecovery({
  db,
  driver,
  deploymentId: input.deploymentId,
  now,
  readRequestEvent: async (sessionId, index) => {
    if (input.requestReadFailures && input.requestReadFailures-- > 0) {
      await checkpoint("request_read_failed");
      throw Error("private-control-error");
    }
    if (sessionId !== input.request.sessionId || index !== BigInt(input.eventIdx ?? "1"))
      return undefined;
    return {
      idx: index,
      kind: "tool_call_requested",
      payloadJson: JSON.stringify({
        run_id: input.request.runId,
        tool_call_id: input.request.toolCallId,
        name: input.request.modelToolName,
        args_json: input.request.originalArguments,
      }),
    };
  },
  completer: {
    async completeToolCall(request) {
      await checkpoint("before_enqueue");
      const ack = new Promise<void>((resolve) => completions.push(resolve));
      process.stdout.write(`COMPLETE ${JSON.stringify(request)}\n`);
      await ack;
      await checkpoint("after_enqueue_ack");
    },
  },
});
installMcpRecovery(runtime);
assertSweepPoliciesExhaustive();
DBOS.setConfig({
  name: "mcp-restart-test",
  systemDatabaseUrl: system,
  executorID: input.workflowId,
  applicationVersion: "mcp-restart-v1",
  runAdminServer: false,
});
await DBOS.launch();
const scanner = makeMcpRecoveryScanner({
  deploymentId: input.deploymentId,
  journal,
  runtime: () => runtime,
  now,
});
if (!input.resume) {
  const consumer = makeToolConsumer({
    registry: createToolRegistry(),
    pendingCalls: makePendingToolCallStore(db),
    now,
    startWorkflow: async () => {
      throw Error("static workflow used");
    },
    startMcpRequest: startMcpRequestWorkflow,
  });
  await consumer.handle(
    {
      idx: BigInt(input.eventIdx ?? "1"),
      kind: "tool_call_requested",
      payloadJson: JSON.stringify({
        run_id: input.request.runId,
        tool_call_id: input.request.toolCallId,
        name: input.request.modelToolName,
        args_json: input.request.originalArguments,
      }),
    },
    { sessionId: input.request.sessionId },
  );
}
await scanner.start();
process.stdout.write("READY\n");
setInterval(() => {}, 1000);
