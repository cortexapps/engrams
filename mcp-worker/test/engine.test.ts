import assert from "node:assert/strict";
import { test } from "node:test";
import { runOperation, discoveredTool } from "../src/engine.ts";
import { startMcpFixture } from "./fixtures/server.ts";
import type { WorkerOperation } from "../src/contracts.ts";
const execute = (
  endpoint: string,
): Extract<WorkerOperation, { kind: "execute" }> => ({
  kind: "execute",
  endpoint,
  tool: "write_counter",
  arguments: { delta: 2 },
  inputSchema: {
    type: "object",
    properties: { delta: { type: "integer" } },
    required: ["delta"],
  },
  argumentConstraintDocuments: [],
  classification: "write",
});
for (const version of ["2026-07-28", "2025-11-25", "2025-06-18"] as const)
  for (const reply of ["json", "sse"] as const)
    test(`worker uses SDK with bounded discovery and exact results ${version} ${reply}`, async (t) => {
      const fixture = await startMcpFixture({ version, reply });
      t.after(() => fixture.close());
      const tools = await runOperation(
        { kind: "discover", endpoint: fixture.url },
        fixture.fetch,
        AbortSignal.timeout(20000),
      );
      assert.equal(Array.isArray(tools) && tools.length, 2);
      const result = await runOperation(
        execute(fixture.url),
        fixture.fetch,
        AbortSignal.timeout(20000),
      );
      assert.deepEqual(result, {
        outcome: "succeeded",
        delivery: "available",
        output: {
          content: [{ type: "text", text: "count=2" }],
          structuredContent: { count: 2 },
          isError: false,
        },
      });
      assert.equal(fixture.writes, 1);
      if (version !== "2026-07-28")
        assert.notEqual(
          fixture.requests.find((r) => r.method === "tools/list")?.sessionId,
          fixture.requests.find((r) => r.method === "tools/call")?.sessionId,
        );
    });
for (const response of [
  "image",
  "audio",
  "resource",
  "resource_link",
  "malformed",
  "binary",
  "error",
] as const)
  test(`worker retains upstream outcome independently of ${response} delivery`, async (t) => {
    const fixture = await startMcpFixture({ response });
    t.after(() => fixture.close());
    const result = await runOperation(
      execute(fixture.url),
      fixture.fetch,
      AbortSignal.timeout(20000),
    );
    assert.equal(fixture.writes, 1);
    assert.ok(
      typeof result === "object" && result !== null && "outcome" in result,
    );
    assert.equal(
      result.outcome,
      ["malformed", "binary"].includes(response)
        ? "outcome_unknown"
        : response === "error"
          ? "failed"
          : "succeeded",
    );
    assert.equal(
      "delivery" in result && result.delivery,
      response === "error" ? "available" : "unavailable",
    );
  });
test("worker rejects input before a tools call and cancels held I/O", async (t) => {
  const fixture = await startMcpFixture({ holdCalls: true });
  t.after(() => fixture.close());
  await assert.rejects(
    runOperation(
      {
        ...execute(fixture.url),
        argumentConstraintDocuments: [
          { properties: { delta: { maximum: 1 } } },
        ],
      },
      fixture.fetch,
      AbortSignal.timeout(20000),
    ),
  );
  assert.equal(fixture.writes, 0);
  const controller = new AbortController();
  const pending = runOperation(
    execute(fixture.url),
    fixture.fetch,
    controller.signal,
  );
  await fixture.waitForCall();
  controller.abort();
  const result = await pending;
  assert.equal(
    typeof result === "object" &&
      result !== null &&
      "outcome" in result &&
      result.outcome,
    "outcome_unknown",
  );
  await fixture.waitForCancellation();
  assert.equal(fixture.writes, 1);
});

for (const attack of [
  "header_mismatch",
  "input_required",
  "version_correction",
  "unauthorized",
] as const)
  test(`write is sent once after ${attack}`, async (t) => {
    const fixture = await startMcpFixture();
    t.after(() => fixture.close());
    let calls = 0;
    const fetch: typeof globalThis.fetch = async (input, init) => {
      const response = await fixture.fetch(input, init);
      const message =
        typeof init?.body === "string" ? JSON.parse(init.body) : {};
      if (message.method !== "tools/call") return response;
      calls++;
      await response.body?.cancel();
      const body =
        attack === "input_required"
          ? {
              jsonrpc: "2.0",
              id: message.id,
              result: {
                resultType: "input_required",
                inputRequests: {},
                requestState: "state",
              },
            }
          : {
              jsonrpc: "2.0",
              id: message.id,
              error: {
                code: attack === "header_mismatch" ? -32021 : -32022,
                message: "credential-secret",
              },
            };
      return new Response(JSON.stringify(body), {
        status: attack === "unauthorized" ? 401 : 200,
        headers: { "content-type": "application/json" },
      });
    };
    const result = await runOperation(
      execute(fixture.url),
      fetch,
      AbortSignal.timeout(20000),
    );
    assert.equal(fixture.writes, 1);
    assert.equal(calls, 1);
    assert.equal(JSON.stringify(result).includes("credential-secret"), false);
    assert.ok(
      typeof result === "object" &&
        result !== null &&
        "outcome" in result &&
        result.outcome === "outcome_unknown",
    );
  });
test("discovery redacts descriptions and rejects secret-bearing names and schemas", async (t) => {
  const fixture = await startMcpFixture();
  t.after(() => fixture.close());
  let mode = "description";
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const response = await fixture.fetch(input, init);
    const request = typeof init?.body === "string" ? JSON.parse(init.body) : {};
    if (request.method !== "tools/list") return response;
    const body = await response.json();
    if (mode === "description")
      body.result.tools[0].description = "echo provider-secret";
    if (mode === "schema")
      body.result.tools[0].inputSchema.const = "provider-secret";
    if (mode === "name") body.result.tools[0].name = "provider-secret";
    return new Response(JSON.stringify(body), {
      headers: { "content-type": "application/json" },
    });
  };
  const operation: WorkerOperation = {
    kind: "discover",
    endpoint: fixture.url,
    credential: { header: "authorization", value: "Bearer provider-secret" },
  };
  const tools = await runOperation(
    operation,
    fetch,
    AbortSignal.timeout(20000),
  );
  assert.ok(Array.isArray(tools));
  assert.equal(tools[0].description, "echo [redacted]");
  for (mode of ["schema", "name"])
    await assert.rejects(
      runOperation(operation, fetch, AbortSignal.timeout(20000)),
      /discovery_failed/,
    );
});
test("output schemas and normalized limits do not erase established success", async (t) => {
  const fixture = await startMcpFixture();
  t.after(() => fixture.close());
  const result = await runOperation(
    {
      ...execute(fixture.url),
      outputSchema: { type: "object", properties: { count: { const: 999 } } },
    },
    fixture.fetch,
    AbortSignal.timeout(20000),
  );
  assert.deepEqual(result, {
    outcome: "succeeded",
    delivery: "unavailable",
    reason: "output_schema",
  });
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const response = await fixture.fetch(input, init);
    const request = typeof init?.body === "string" ? JSON.parse(init.body) : {};
    if (request.method !== "tools/call") return response;
    const body = await response.json();
    body.result.content = [{ type: "text", text: "x".repeat(1024 * 1024) }];
    return new Response(JSON.stringify(body), {
      headers: { "content-type": "application/json" },
    });
  };
  assert.deepEqual(
    await runOperation(execute(fixture.url), fetch, AbortSignal.timeout(20000)),
    { outcome: "succeeded", delivery: "unavailable", reason: "result_limit" },
  );
  assert.equal(fixture.writes, 2);
});
test("secret echoes in structured output and text are redacted", async (t) => {
  const fixture = await startMcpFixture();
  t.after(() => fixture.close());
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const response = await fixture.fetch(input, init);
    const request = typeof init?.body === "string" ? JSON.parse(init.body) : {};
    if (request.method !== "tools/call") return response;
    const body = await response.json();
    body.result.content = [{ type: "text", text: "echo provider-secret" }];
    body.result.structuredContent = { value: "provider-secret" };
    return new Response(JSON.stringify(body), {
      headers: { "content-type": "application/json" },
    });
  };
  const result = await runOperation(
    {
      ...execute(fixture.url),
      credential: { header: "x-api-key", value: "provider-secret" },
    },
    fetch,
    AbortSignal.timeout(20000),
  );
  assert.equal(JSON.stringify(result).includes("provider-secret"), false);
  assert.ok(JSON.stringify(result).includes("[redacted]"));
});
test("schema secret detection examines strings rather than serialized escapes", () => {
  const secret = 'key"with\\escape';
  assert.throws(
    () =>
      discoveredTool(
        {
          name: "read",
          inputSchema: {
            type: "object",
            properties: { value: { const: secret } },
          },
        },
        [secret],
      ),
    /secret_in_definition/,
  );
});
test("unusable deeply nested structured output retains established success", async (t) => {
  const fixture = await startMcpFixture();
  t.after(() => fixture.close());
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const response = await fixture.fetch(input, init);
    const request = typeof init?.body === "string" ? JSON.parse(init.body) : {};
    if (request.method !== "tools/call") return response;
    await response.body?.cancel();
    const nested = '{"child":'.repeat(10000) + "0" + "}".repeat(10000);
    return new Response(
      `{"jsonrpc":"2.0","id":${JSON.stringify(request.id)},"result":{"resultType":"complete","content":[],"isError":false,"structuredContent":${nested}}}`,
      { headers: { "content-type": "application/json" } },
    );
  };
  const result = await runOperation(
    execute(fixture.url),
    fetch,
    AbortSignal.timeout(20000),
  );
  assert.ok(
    typeof result === "object" &&
      result !== null &&
      "outcome" in result &&
      result.outcome === "succeeded",
  );
  assert.ok("delivery" in result && result.delivery === "unavailable");
  assert.equal(fixture.writes, 1);
});
