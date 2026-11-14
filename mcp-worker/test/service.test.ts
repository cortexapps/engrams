import assert from "node:assert/strict";
import { test } from "node:test";
import { randomUUID } from "node:crypto";
import { createWorker } from "../src/service.ts";
import { digest } from "../src/encoding.ts";
import { ZonePolicy } from "../src/network.ts";
import { workerTls, localRpc } from "./fixtures/worker-tls.ts";
import type {
  WorkerIdentity,
  WorkerOperation,
  WorkerRequest,
} from "../src/contracts.ts";
const zone = () =>
  new ZonePolicy({
    id: "public",
    revision: "1",
    mode: "public",
    authorities: ["tools.example"],
    aliases: [],
    cidrs: [],
    denyCidrs: [],
  });
function envelope(
  identity: WorkerIdentity,
  operation: WorkerOperation,
): WorkerRequest {
  return {
    protocol: 1,
    intent: {
      id: randomUUID(),
      ...identity,
      connectionId: "connection",
      connectionRevision: 1,
      credentialIdentity: "credential",
      scope: {
        kind: "administration",
        requestId: "admin-request",
        actorId: "actor",
      },
      operation: operation.kind,
      destination: "endpoint" in operation ? operation.endpoint : "",
      requestDigest: "a".repeat(64),
      operationDigest: digest(operation),
      expiresAt: Date.now() + 10000,
      lease: {
        id: randomUUID(),
        deploymentId: "deployment",
        expiresAt: Date.now() + 10000,
      },
    },
    operation,
  };
}
test("mTLS RPC binds boot, policy, payload and lease; each intent is single use", async (t) => {
  const tls = await workerTls();
  const worker = createWorker({
    zone: zone(),
    tls,
    callers: [{ fingerprint: tls.fingerprint, role: "orchestrator" }],
    enabled: true,
  });
  const port = await worker.listen(0, "127.0.0.1");
  t.after(() => worker.close());
  await assert.rejects(localRpc(port, tls, "/identity", undefined, false));
  const hello = await localRpc(port, tls, "/identity");
  assert.equal(hello.status, 200);
  assert.deepEqual(hello.body, worker.identity);
  const op: WorkerOperation = {
    kind: "validateSchemas",
    documents: [{ type: "integer" }],
    value: 1,
  };
  for (const field of ["bootId", "policyDigest", "operationDigest"] as const) {
    const request = envelope(worker.identity, op);
    request.intent[field] = "wrong";
    assert.equal((await localRpc(port, tls, "/rpc", request)).status, 403);
  }
  const expired = envelope(worker.identity, op);
  expired.intent.lease.expiresAt = 0;
  assert.equal((await localRpc(port, tls, "/rpc", expired)).status, 403);
  const request = envelope(worker.identity, op);
  const result = await localRpc(port, tls, "/rpc", request);
  assert.equal(result.status, 200);
  assert.ok(
    typeof result.body === "object" &&
      result.body !== null &&
      "intent" in result.body,
  );
  assert.deepEqual(result.body.intent, request.intent);
  assert.ok(
    typeof result.body === "object" &&
      result.body !== null &&
      "result" in result.body &&
      result.body.result === true,
  );
  assert.equal((await localRpc(port, tls, "/rpc", request)).status, 403);
  assert.equal((await localRpc(port, tls, "/proxy", request)).status, 404);
  assert.equal(
    (
      await localRpc(
        port,
        tls,
        "/rpc",
        envelope(worker.identity, {
          kind: "token",
          endpoint: "https://tools.example/token",
          grant: {
            grant_type: "refresh_token",
            refresh_token: "secret",
            client_id: "client",
            resource: "https://tools.example/mcp",
          },
        }),
      )
    ).status,
    403,
  );
});
test("disabled worker and unknown client identity cannot dispatch", async (t) => {
  const tls = await workerTls();
  const disabled = createWorker({
    zone: zone(),
    tls,
    callers: [{ fingerprint: tls.fingerprint, role: "orchestrator" }],
  });
  const port = await disabled.listen(0, "127.0.0.1");
  t.after(() => disabled.close());
  assert.equal(
    (
      await localRpc(
        port,
        tls,
        "/rpc",
        envelope(disabled.identity, { kind: "validateSchemas", documents: [] }),
      )
    ).status,
    503,
  );
  const unknown = createWorker({
    zone: zone(),
    tls,
    callers: [{ fingerprint: "00", role: "orchestrator" }],
    enabled: true,
  });
  const other = await unknown.listen(0, "127.0.0.1");
  t.after(() => unknown.close());
  assert.equal((await localRpc(other, tls, "/identity")).status, 403);
});
test("execution schema validation keeps the orchestrator role and exact bound scope", async (t) => {
  const tls = await workerTls();
  for (const role of ["orchestrator", "coordinator"] as const) {
    let dials = 0;
    const worker = createWorker({
      zone: zone(),
      tls,
      callers: [{ fingerprint: tls.fingerprint, role }],
      enabled: true,
      network: {
        resolve: async () => {
          dials++;
          throw Error("unexpected provider lookup");
        },
        request: () => {
          dials++;
          throw Error("unexpected provider request");
        },
      },
    });
    const port = await worker.listen(0, "127.0.0.1");
    t.after(() => worker.close());
    const request = envelope(worker.identity, {
      kind: "validateSchemas",
      documents: [{ type: "integer" }],
      value: 2,
    });
    request.intent.scope = {
      kind: "execution",
      sessionId: "session",
      runId: "run",
      toolCallId: "call",
      bindingHash: "b".repeat(64),
    };
    const result = await localRpc(port, tls, "/rpc", request);
    assert.equal(result.status, role === "orchestrator" ? 200 : 403);
    if (role === "orchestrator") {
      assert.ok(
        typeof result.body === "object" &&
          result.body !== null &&
          "intent" in result.body,
      );
      assert.deepEqual(result.body.intent, request.intent);
      assert.equal((await localRpc(port, tls, "/rpc", request)).status, 403);
      const credential = { header: "authorization", value: "Bearer private" };
      const secretRequest = envelope(worker.identity, {
        kind: "validateSchemas",
        documents: [{}],
      });
      const invalid = {
        ...secretRequest,
        operation: { ...secretRequest.operation, credential },
      };
      assert.equal((await localRpc(port, tls, "/rpc", invalid)).status, 400);
    }
    assert.equal(dials, 0);
  }
});
test("OAuth admission is separate, destination validation does not dial, and scoped cancel aborts I/O", async (t) => {
  const { createServer, request } = await import("node:https");
  const tls = await workerTls();
  let hits = 0;
  let arrived: () => void = () => {};
  const arrival = new Promise<void>((resolve) => {
    arrived = resolve;
  });
  const provider = createServer(tls, () => {
    hits++;
    arrived();
  });
  await new Promise<void>((resolve) =>
    provider.listen(0, "127.0.0.1", resolve),
  );
  t.after(() => {
    provider.closeAllConnections();
    return new Promise<void>((resolve) => provider.close(() => resolve()));
  });
  const address = provider.address();
  assert.ok(address && typeof address !== "string");
  const policy = new ZonePolicy({
    id: "public",
    revision: "1",
    mode: "public",
    authorities: ["tools.example"],
    aliases: [],
    cidrs: [],
    denyCidrs: [],
    ca: tls.ca,
    limits: { oauth: 1 },
  });
  const worker = createWorker({
    zone: policy,
    tls,
    callers: [{ fingerprint: tls.fingerprint, role: "coordinator" }],
    enabled: true,
    network: {
      resolve: async () => ["8.8.8.8"],
      request: (url, options) =>
        request(url, { ...options, hostname: "127.0.0.1", port: address.port }),
    },
  });
  const port = await worker.listen(0, "127.0.0.1");
  t.after(() => worker.close());
  const first = envelope(worker.identity, {
    kind: "metadata",
    endpoint: "https://tools.example/metadata",
  });
  const pending = localRpc(port, tls, "/rpc", first);
  void pending.catch(() => {});
  await arrival;
  assert.equal(
    (
      await localRpc(
        port,
        tls,
        "/rpc",
        envelope(worker.identity, {
          kind: "metadata",
          endpoint: "https://tools.example/metadata",
        }),
      )
    ).status,
    429,
  );
  assert.equal(
    (
      await localRpc(
        port,
        tls,
        "/rpc",
        envelope(worker.identity, {
          kind: "validateDestination",
          endpoint: "https://tools.example/metadata",
        }),
      )
    ).status,
    200,
  );
  assert.equal(hits, 1);
  const wrong = envelope(worker.identity, {
    kind: "cancel",
    targetIntentId: first.intent.id,
  });
  wrong.intent.scope = {
    kind: "administration",
    requestId: "different",
    actorId: "actor",
  };
  assert.equal((await localRpc(port, tls, "/rpc", wrong)).status, 403);
  assert.equal(
    (
      await localRpc(
        port,
        tls,
        "/rpc",
        envelope(worker.identity, {
          kind: "cancel",
          targetIntentId: first.intent.id,
        }),
      )
    ).status,
    200,
  );
  assert.equal((await pending).status, 422);
  assert.equal(hits, 1);
});
