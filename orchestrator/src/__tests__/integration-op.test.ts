/**
 * The IntegrationOp seam: spec-building + delegation (run-op.ts) and the generic
 * SDK seam (clients.ts). Uses a fake coordinator client + an empty custom-connector
 * source so the real built-in seeds (github/datadog) drive the credential spec.
 */

import { expect, test, describe, beforeEach } from "bun:test";
import { create } from "@bufbuild/protobuf";

import { runIntegrationOp, resolveIntegrationCredential, type IntegrationOpClient } from "../integrations/run-op.ts";
import {
  asBearer,
  asHeaders,
  registerIntegrationClient,
  getIntegrationClient,
  invalidateIntegrationClient,
} from "../integrations/clients.ts";
import { invalidateRegistry } from "../connectors/registry.ts";
import {
  RunIntegrationOpResponseSchema,
  ResolveIntegrationCredentialResponseSchema,
  ResolvedCredentialSchema,
  type RunIntegrationOpRequest,
  type ResolveIntegrationCredentialRequest,
} from "../gen/engram/app/v1/integration_op_pb.ts";

/** No custom DB connectors — loadRegistry falls back to the built-in seeds. */
const emptySource = { list: async () => [] };

interface Recorder {
  run: RunIntegrationOpRequest[];
  resolve: ResolveIntegrationCredentialRequest[];
}

function fakeClient(
  resolved?: { case: "bearer"; token: string } | { case: "headers"; values: Record<string, string> },
): { client: IntegrationOpClient; calls: Recorder } {
  const calls: Recorder = { run: [], resolve: [] };
  const credential =
    resolved?.case === "headers"
      ? create(ResolvedCredentialSchema, { cred: { case: "headers", value: { values: resolved.values } } })
      : create(ResolvedCredentialSchema, {
          cred: { case: "bearer", value: { token: resolved?.token ?? "xoxb-default" } },
        });
  const client = {
    runIntegrationOp: async (req: RunIntegrationOpRequest) => {
      calls.run.push(req);
      return create(RunIntegrationOpResponseSchema, {
        status: 200,
        body: new TextEncoder().encode('{"ok":true}'),
        contentType: "application/json",
        truncated: false,
      });
    },
    resolveIntegrationCredential: async (req: ResolveIntegrationCredentialRequest) => {
      calls.resolve.push(req);
      return create(ResolveIntegrationCredentialResponseSchema, { credential });
    },
  } as unknown as IntegrationOpClient;
  return { client, calls };
}

describe("runIntegrationOp", () => {
  beforeEach(() => invalidateRegistry());

  test("inject connector → resolved spec carries host + every injected secretRef", async () => {
    const { client, calls } = fakeClient();
    const res = await runIntegrationOp(
      "datadog",
      { method: "GET", path: "/api/v1/dashboard" },
      { connectors: emptySource, integrationOp: client },
    );
    expect(res.status).toBe(200);
    expect(calls.run).toHaveLength(1);
    const req = calls.run[0]!;
    expect(req.host).toBe("api.datadoghq.com");
    expect(req.method).toBe("GET");
    expect(req.path).toBe("/api/v1/dashboard");
    expect(req.credential?.source).toBe("inject");
    // The API host receives the REST spellings only. MCP uses the same stored
    // keys under underscore headers, but those aliases are host-scoped.
    const refs = (req.credential?.injects ?? []).map((i) => i.secretRef).sort();
    expect(refs).toEqual(["datadog-api-key", "datadog-app-key"]);
  });

  test("mint connector → spec carries the provider id (not the kind)", async () => {
    const { client, calls } = fakeClient();
    await runIntegrationOp(
      "github",
      { method: "POST", path: "/repos/o/r/issues", body: '{"title":"hi"}' },
      { connectors: emptySource, integrationOp: client },
    );
    const req = calls.run[0]!;
    expect(req.credential?.source).toBe("mint");
    expect(req.credential?.mintProvider).toBe("github");
    // String body is UTF-8 encoded onto the wire.
    expect(new TextDecoder().decode(req.body)).toBe('{"title":"hi"}');
  });

  test("unknown connector throws", async () => {
    const { client } = fakeClient();
    await expect(
      runIntegrationOp("nope", { method: "GET", path: "/" }, { connectors: emptySource, integrationOp: client }),
    ).rejects.toThrow(/unknown connector/);
  });
});

describe("resolveIntegrationCredential", () => {
  beforeEach(() => invalidateRegistry());

  test("returns the coordinator-resolved credential", async () => {
    const { client, calls } = fakeClient({ case: "bearer", token: "xoxb-abc" });
    const cred = await resolveIntegrationCredential("datadog", { connectors: emptySource, integrationOp: client });
    expect(calls.resolve).toHaveLength(1);
    expect(asBearer(cred)).toBe("xoxb-abc");
  });
});

describe("the generic SDK seam (clients.ts)", () => {
  beforeEach(() => {
    invalidateRegistry();
    invalidateIntegrationClient("datadog");
  });

  test("asBearer / asHeaders extract the right shape", () => {
    const bearer = create(ResolvedCredentialSchema, { cred: { case: "bearer", value: { token: "t" } } });
    expect(asBearer(bearer)).toBe("t");
    expect(asHeaders(bearer)).toEqual({ Authorization: "Bearer t" });

    const headers = create(ResolvedCredentialSchema, {
      cred: { case: "headers", value: { values: { "DD-API-KEY": "k", "DD-APPLICATION-KEY": "a" } } },
    });
    expect(asHeaders(headers)).toEqual({ "DD-API-KEY": "k", "DD-APPLICATION-KEY": "a" });
    expect(() => asBearer(headers)).toThrow(/expected a bearer/);
  });

  test("getIntegrationClient builds via the adapter and caches by provider", async () => {
    const { client, calls } = fakeClient({ case: "headers", values: { "DD-API-KEY": "k", "DD-APPLICATION-KEY": "a" } });
    let built = 0;
    registerIntegrationClient("datadog", (cred) => {
      built++;
      return { keys: asHeaders(cred) };
    });
    const deps = { connectors: emptySource, integrationOp: client };
    const c1 = await getIntegrationClient<{ keys: Record<string, string> }>("datadog", deps);
    const c2 = await getIntegrationClient<{ keys: Record<string, string> }>("datadog", deps);
    expect(c1.keys["DD-API-KEY"]).toBe("k");
    expect(c2).toBe(c1); // cached instance
    expect(built).toBe(1); // adapter ran once
    expect(calls.resolve).toHaveLength(1); // credential resolved once
  });

  test("getIntegrationClient throws for an unregistered provider", async () => {
    const { client } = fakeClient();
    await expect(
      getIntegrationClient("github", { connectors: emptySource, integrationOp: client }),
    ).rejects.toThrow(/no integration client registered/);
  });
});
