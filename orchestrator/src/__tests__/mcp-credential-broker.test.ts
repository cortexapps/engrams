import { unwrapCredentialControlClient } from "../mcp/control-client.ts";
import { expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";
import { create } from "@bufbuild/protobuf";
import {
  CredentialControlService,
  CredentialKind,
  CredentialStatusSchema,
} from "../gen/engram/mcp/v1/control_pb.ts";
import {
  makeCredentialBroker,
  type AdmittedCredentialOperation,
} from "../mcp/credential-broker.ts";
import type { WorkerRequest } from "../../../mcp-worker/src/contracts.ts";
import { canonicalDigest } from "../mcp/hash.ts";
const now = 100000;
const identity = {
  protocol: 1 as const,
  bootId: "boot",
  zoneId: "public",
  policyDigest: "a".repeat(64),
  policyRevision: "v1",
};
const status = create(CredentialStatusSchema, {
  protocol: 1,
  context: {
    connectionId: "connection",
    configRevision: 2n,
    authorizationGeneration: 3n,
    credentialIdentity: "identity",
    resource: { zoneId: "public", endpoint: "https://tools.example.test/mcp" },
    policyDigest: identity.policyDigest,
    policyRevision: "v1",
  },
  reference: {
    kind: CredentialKind.API_KEY,
    name: "mcp-key",
    connectionId: "connection",
    generation: 1n,
  },
  status: "connected",
  version: 1n,
});
function fixture(expire = false, finalFenceMs = 0, responseDigest?: string) {
  let time = now,
    deliveries = 0,
    leases = 0;
  const seen: unknown[] = [];
  const admitted: AdmittedCredentialOperation[] = [];
  const requests: WorkerRequest[] = [];
  const control = unwrapCredentialControlClient(
    createClient(
      CredentialControlService,
      createRouterTransport((router) =>
        router.service(CredentialControlService, {
          async lease(req) {
            leases++;
            seen.push(req);
            // Match McpAuthority::lease: exactly 64 ASCII hexadecimal characters.
            if (req.requestDigest.length !== 64 || !/^[0-9a-fA-F]{64}$/.test(req.requestDigest))
              throw new ConnectError("invalid request digest", Code.PermissionDenied);
            if (expire) time = now + 10000;
            return {
              protocol: 1,
              header: "x-api-key",
              value: "transient-value",
              expiresAt: req.expiresAt,
              operationExpiresAt: req.operationExpiresAt,
              intentId: req.intentId,
            };
          },
        }),
      ),
    ),
  );
  const broker = makeCredentialBroker({
    control,
    now: () => time,
    async beforeDispatch() {
      time += finalFenceMs;
    },
    admission: {
      async assertAdmitted(operation) {
        admitted.push(structuredClone(operation));
      },
    },
    worker: {
      async identity() {
        return identity;
      },
      async dispatch(request) {
        deliveries++;
        requests.push(structuredClone(request));
        return {
          ok: true,
          identity,
          intent: request.intent,
          intentId: request.intent.id,
          requestDigest: responseDigest ?? request.intent.requestDigest,
          operationDigest: request.intent.operationDigest,
          leaseId: request.intent.lease.id,
          result: { outcome: "succeeded" },
        };
      },
    },
  });
  const input: AdmittedCredentialOperation = {
    status,
    identity,
    intent: {
      id: "intent",
      bootId: "boot",
      zoneId: "public",
      policyDigest: identity.policyDigest,
      connectionId: "connection",
      connectionRevision: 2,
      credentialIdentity: "identity",
      scope: {
        kind: "execution" as const,
        sessionId: "s",
        runId: "r",
        toolCallId: "call",
        bindingHash: "b".repeat(64),
      },
      operation: "execute" as const,
      destination: "https://tools.example.test/mcp",
      requestDigest: `sha256:${"b".repeat(64)}`,
      expiresAt: now + 60000,
      lease: { id: "admission", deploymentId: "deployment", expiresAt: now + 60000 },
    },
    operation: {
      kind: "execute" as const,
      endpoint: "https://tools.example.test/mcp",
      tool: "Read",
      arguments: {},
      inputSchema: { type: "object" },
      argumentConstraintDocuments: [],
      classification: "read" as const,
    },
  };
  return { broker, input, seen, admitted, requests, counters: () => ({ deliveries, leases }) };
}
for (const [operation, rawDigest] of [
  ["discover", "1d8346198aa04c4188491657f648995a8657213ae8bde54920a7588ca74f778e"],
  ["execute", "4307c51da41ceb7da86053004e56d21d4b7b6fe78811d8ce64a2e1468b936a77"],
] as const)
  test(`${operation} leases raw hex while admission and worker retain the canonical intent`, async () => {
    const f = fixture();
    f.input.intent.requestDigest = canonicalDigest({ kind: operation });
    if (operation === "discover") {
      f.input.intent.operation = operation;
      f.input.intent.scope = { kind: "administration", requestId: "discovery", actorId: "admin" };
      f.input.operation = { kind: operation, endpoint: "https://tools.example.test/mcp" };
    }
    const original = structuredClone(f.input);
    const response = await f.broker.dispatch(f.input);
    expect(f.counters()).toEqual({ deliveries: 1, leases: 1 });
    expect(f.seen[0]).toMatchObject({
      protocol: 1,
      context: original.status.context,
      reference: original.status.reference,
      worker: original.identity,
      intentId: "intent",
      requestDigest: rawDigest,
      operation,
      expiresAt: 110000n,
      operationExpiresAt: 160000n,
      admission: { id: "admission", deploymentId: "deployment", expiresAt: 160000n },
      scope:
        operation === "discover"
          ? { case: "administration", value: { requestId: "discovery", actorId: "admin" } }
          : {
              case: "execution",
              value: {
                sessionId: "s",
                runId: "r",
                toolCallId: "call",
                bindingHash: "b".repeat(64),
              },
            },
    });
    expect(f.admitted).toEqual([original, original]);
    expect(response.ok).toBe(true);
    if (!response.ok) throw Error("unexpected worker failure");
    expect(f.requests).toEqual([
      {
        protocol: 1,
        intent: { ...original.intent, operationDigest: response.operationDigest },
        operation: {
          ...original.operation,
          credential: { header: "x-api-key", value: "transient-value" },
        },
      },
    ]);
    expect(response.requestDigest).toBe(`sha256:${rawDigest}`);
    expect(response.intent).toEqual(f.requests[0]!.intent);
    expect(f.input).toEqual(original);
  });

test("malformed canonical request digests fail before credential leasing", async () => {
  const raw = "b".repeat(64);
  for (const requestDigest of [
    "",
    raw,
    `sha256:sha256:${raw}`,
    `SHA256:${raw}`,
    `sha256:${raw.toUpperCase()}`,
    `sha256:${raw} `,
    `sha256:${raw}\n`,
    `sha256:${raw.slice(1)}`,
    `sha256:${raw}0`,
    `sha256:${"g".repeat(64)}`,
  ]) {
    const f = fixture();
    f.input.intent.requestDigest = requestDigest;
    await expect(f.broker.dispatch(f.input)).rejects.toThrow("MCP request digest invalid");
    expect(f.counters()).toEqual({ deliveries: 0, leases: 0 });
  }
});

test("raw control digest cannot replace the tagged worker response binding", async () => {
  const f = fixture(false, 0, "b".repeat(64));
  await expect(f.broker.dispatch(f.input)).rejects.toThrow("MCP worker response mismatch");
  expect(f.counters()).toEqual({ deliveries: 1, leases: 1 });
});

test("broker preserves 60 second intent while credential delivery expires in 10 seconds", async () => {
  const f = fixture();
  await f.broker.dispatch(f.input);
  expect(f.counters()).toEqual({ deliveries: 1, leases: 1 });
  expect(f.seen[0]).toMatchObject({ expiresAt: 110000n, operationExpiresAt: 160000n });
});
test("expired handoff causes no resource call and no re-lease", async () => {
  const f = fixture(true);
  await expect(f.broker.dispatch(f.input)).rejects.toThrow("expired");
  expect(f.counters()).toEqual({ deliveries: 0, leases: 1 });
});
test("worker policy and boot mismatch prevents secret lease", async () => {
  const f = fixture();
  f.input.identity = { ...identity, bootId: "wrong" };
  await expect(f.broker.dispatch(f.input)).rejects.toThrow();
  expect(f.counters()).toEqual({ deliveries: 0, leases: 0 });
});
test("admission rejection prevents worker and credential access", async () => {
  const f = fixture();
  let touched = false;
  const control = unwrapCredentialControlClient(
    createClient(
      CredentialControlService,
      createRouterTransport((router) =>
        router.service(CredentialControlService, {
          async lease() {
            touched = true;
            throw new Error("unexpected");
          },
        }),
      ),
    ),
  );
  const broker = makeCredentialBroker({
    control,
    now: () => now,
    admission: {
      async assertAdmitted() {
        throw new Error("admission denied");
      },
    },
    worker: {
      async identity() {
        touched = true;
        return identity;
      },
      async dispatch() {
        touched = true;
        throw new Error("unexpected");
      },
    },
  });
  await expect(broker.dispatch(f.input)).rejects.toThrow("admission denied");
  expect(touched).toBe(false);
});
test("changed destination, credential identity and insufficient operation admission fail before leasing", async () => {
  for (const mutate of [
    (f: ReturnType<typeof fixture>) => {
      f.input.operation.endpoint = "https://other.example.test/";
    },
    (f: ReturnType<typeof fixture>) => {
      f.input.intent.credentialIdentity = "old";
    },
    (f: ReturnType<typeof fixture>) => {
      f.input.intent.lease.expiresAt--;
    },
  ]) {
    const f = fixture();
    mutate(f);
    await expect(f.broker.dispatch(f.input)).rejects.toThrow();
    expect(f.counters()).toEqual({ deliveries: 0, leases: 0 });
  }
});
test("authority invalidated after lease is checked again before resource dispatch", async () => {
  const f = fixture();
  let checks = 0,
    sent = false;
  const control = unwrapCredentialControlClient(
    createClient(
      CredentialControlService,
      createRouterTransport((router) =>
        router.service(CredentialControlService, {
          async lease(req) {
            return {
              protocol: 1,
              header: "x-api-key",
              value: "secret",
              expiresAt: req.expiresAt,
              operationExpiresAt: req.operationExpiresAt,
              intentId: req.intentId,
            };
          },
        }),
      ),
    ),
  );
  const broker = makeCredentialBroker({
    control,
    now: () => now,
    admission: {
      async assertAdmitted() {
        if (++checks === 2) throw new Error("revoked");
      },
    },
    worker: {
      async identity() {
        return identity;
      },
      async dispatch(request) {
        sent = true;
        return {
          ok: true,
          identity,
          intent: request.intent,
          intentId: request.intent.id,
          requestDigest: request.intent.requestDigest,
          operationDigest: request.intent.operationDigest,
          leaseId: request.intent.lease.id,
          result: {},
        };
      },
    },
  });
  await expect(broker.dispatch(f.input)).rejects.toThrow("revoked");
  expect(sent).toBe(false);
});
test("worker transport errors cannot expose the transient credential", async () => {
  const f = fixture();
  const control = unwrapCredentialControlClient(
    createClient(
      CredentialControlService,
      createRouterTransport((router) =>
        router.service(CredentialControlService, {
          async lease(req) {
            return {
              protocol: 1,
              header: "x-api-key",
              value: "secret-value",
              expiresAt: req.expiresAt,
              operationExpiresAt: req.operationExpiresAt,
              intentId: req.intentId,
            };
          },
        }),
      ),
    ),
  );
  const broker = makeCredentialBroker({
    control,
    now: () => now,
    admission: { async assertAdmitted() {} },
    worker: {
      async identity() {
        return identity;
      },
      async dispatch() {
        throw new Error("request with secret-value failed");
      },
    },
  });
  await expect(broker.dispatch(f.input)).rejects.toThrow("MCP worker operation unavailable");
});

for (const finalFenceMs of [9999, 10000, 11000])
  test(`final fence at ${finalFenceMs} ms preserves the delivery boundary and full operation deadline`, async () => {
    const f = fixture(false, finalFenceMs);
    if (finalFenceMs < 10000) {
      await f.broker.dispatch(f.input);
      expect(f.counters()).toEqual({ deliveries: 1, leases: 1 });
    } else {
      await expect(f.broker.dispatch(f.input)).rejects.toThrow("credential delivery expired");
      expect(f.counters()).toEqual({ deliveries: 0, leases: 1 });
    }
    expect(f.seen[0]).toMatchObject({ expiresAt: 110000n, operationExpiresAt: 160000n });
    expect(f.input.intent.expiresAt).toBe(160000);
  });
