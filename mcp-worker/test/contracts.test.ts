import assert from "node:assert/strict";
import { test } from "node:test";
import { digest, canonical, parseWorkerRequest } from "../src/index.ts";
import type { WorkerRequest } from "../src/contracts.ts";
const request: WorkerRequest = {
  protocol: 1,
  intent: {
    id: "intent",
    bootId: "boot",
    zoneId: "zone",
    policyDigest: "a".repeat(64),
    connectionId: "connection",
    connectionRevision: 1,
    credentialIdentity: "credential",
    scope: { kind: "administration", requestId: "request", actorId: "actor" },
    operation: "validateSchemas",
    destination: "",
    requestDigest: "b".repeat(64),
    operationDigest: "c".repeat(64),
    expiresAt: 100,
    lease: { id: "lease", deploymentId: "deployment", expiresAt: 100 },
  },
  operation: { kind: "validateSchemas", documents: [{}] },
};
test("wire contract has distinct administration and execution scopes", () => {
  assert.deepEqual(parseWorkerRequest(request), request);
  assert.throws(() =>
    parseWorkerRequest({
      ...request,
      operation: {
        kind: "execute",
        endpoint: "https://tools.example/mcp",
        tool: "write",
        arguments: {},
        inputSchema: {},
        argumentConstraintDocuments: [],
        classification: "write",
      },
    }),
  );
  assert.throws(() =>
    parseWorkerRequest({
      ...request,
      intent: {
        ...request.intent,
        scope: {
          kind: "execution",
          sessionId: "session",
          runId: "run",
          toolCallId: "call",
          bindingHash: "d".repeat(64),
        },
      },
    }),
  );
  assert.throws(() =>
    parseWorkerRequest({
      ...request,
      intent: { ...request.intent, operation: "unknown" },
    }),
  );
});
test("canonical JSON sorts only object keys and operation digest changes with credential bytes", () => {
  assert.equal(
    canonical({ z: [2, 1], a: { c: true, b: "text" }, omitted: undefined }),
    '{"a":{"b":"text","c":true},"z":[2,1]}',
  );
  assert.equal(digest({ a: 1, b: 2 }), digest({ b: 2, a: 1 }));
  assert.notEqual(
    digest({ credential: { value: "old" } }),
    digest({ credential: { value: "new" } }),
  );
  assert.throws(() => canonical({ value: Infinity }));
});
