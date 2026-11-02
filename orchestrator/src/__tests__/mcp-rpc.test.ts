import { describe, expect, test } from "bun:test";
import { createClient, Code, ConnectError } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import { Hono } from "hono";
import type { AddressInfo } from "node:net";
import { buildServer } from "../server.ts";
import { IntegrationService } from "../gen/engram/app/v1/integration_pb.ts";
import { mcpHandlers, type McpServices } from "../mcp/contracts.ts";
import type {
  IntegrationConnectionRow,
  IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import { reviewCatalog } from "../mcp/catalog.ts";

const unavailable = async (): Promise<never> => {
  throw new Error("unexpected service invocation");
};
const row: IntegrationConnectionRow = {
  id: "connection-1",
  alias: "tools",
  provider: "mcp",
  displayName: "Tools",
  isDefault: false,
  enabled: false,
  testedAt: null,
  createdAt: new Date(0),
  updatedAt: new Date(0),
  config: {
    endpoint: "https://tools.example.test/mcp",
    zone: "public",
    auth: { kind: "api_key", secretRef: "mcp.tools.api-key" },
  },
  mcp: {
    configRevision: 1,
    authGeneration: 1,
    credentialIdentity: "account-a",
    catalogRevision: 0,
    revoked: false,
  },
};
const connections: IntegrationConnectionStore = {
  list: async () => [row],
  get: async (id) => (id === row.id ? row : null),
  getMany: async () => [row],
  getDefault: async () => null,
  create: unavailable,
  update: unavailable,
  delete: unavailable,
  markTested: unavailable,
  setEnabled: unavailable,
  setConfig: unavailable,
  ensureDefault: unavailable,
};
function services(): McpServices {
  return {
    enabled: true,
    repository: {
      updateConfig: unavailable,
      setAuthorization: unavailable,
      disconnect: unavailable,
      setEnabled: unavailable,
      saveDraft: unavailable,
      publish: unavailable,
      getCatalog: unavailable,
      listCatalogs: unavailable,
    },
    journal: {
      get: unavailable,
      hasUnknown: unavailable,
      create: unavailable,
      requestApproval: unavailable,
      pending: unavailable,
      decide: unavailable,
      admit: unavailable,
      dispatch: unavailable,
      finish: unavailable,
      cancel: unavailable,
      resolveUnknown: unavailable,
      expire: unavailable,
      expireUnadmitted: unavailable,
    },
  };
}
async function spawn(service?: McpServices, role = "admin", email?: string) {
  const server = buildServer(new Hono(), (router) =>
    router.service(
      IntegrationService,
      mcpHandlers({
        services: service,
        connections,
        getSession: async () => ({ user: { id: "owner", role, email } }),
        now: () => new Date(0),
        connectionToProto: (c) => ({ id: c.id }),
      }),
    ),
  );
  const url = await new Promise<string>((resolve) =>
    server.listen(0, "127.0.0.1", () =>
      resolve(`http://127.0.0.1:${(server.address() as AddressInfo).port}/rpc`),
    ),
  );
  return {
    client: createClient(
      IntegrationService,
      createConnectTransport({ baseUrl: url, httpVersion: "1.1" }),
    ),
    close: () =>
      new Promise<void>((resolve, reject) => server.close((e) => (e ? reject(e) : resolve()))),
  };
}
async function code(promise: Promise<unknown>, expected: Code) {
  try {
    await promise;
    throw new Error("request should fail");
  } catch (e) {
    expect(e).toBeInstanceOf(ConnectError);
    expect((e as ConnectError).code).toBe(expected);
  }
}
describe("MCP native wire contract", () => {
  test("all new methods fail closed by default", async () => {
    const s = await spawn();
    try {
      await code(
        s.client.discoverMcp({ connectionId: row.id, expectedRevision: 1 }),
        Code.FailedPrecondition,
      );
      await code(s.client.putMcpCredential({}), Code.FailedPrecondition);
      await code(s.client.completeMcpAuthorization({}), Code.FailedPrecondition);
      await code(s.client.recoverMcpCredential({}), Code.FailedPrecondition);
      await code(s.client.publishMcp({}), Code.FailedPrecondition);
      await code(s.client.authorizeMcp({}), Code.FailedPrecondition);
      await code(s.client.disconnectMcp({}), Code.FailedPrecondition);
      await code(s.client.listMcpApprovals({}), Code.FailedPrecondition);
      await code(s.client.decideMcpApproval({}), Code.FailedPrecondition);
      await code(s.client.cancelMcpCall({}), Code.FailedPrecondition);
      await code(s.client.resolveMcpOutcome({}), Code.FailedPrecondition);
      await code(s.client.rebindMcp({}), Code.FailedPrecondition);
    } finally {
      await s.close();
    }
  });
  test("admin controls and missing transport fail before services run", async () => {
    const member = await spawn(services(), "user");
    const admin = await spawn(services());
    try {
      await code(member.client.discoverMcp({ connectionId: row.id }), Code.PermissionDenied);
      await code(member.client.publishMcp({}), Code.PermissionDenied);
      await code(member.client.authorizeMcp({}), Code.PermissionDenied);
      await code(admin.client.discoverMcp({ connectionId: row.id }), Code.FailedPrecondition);
      await code(
        admin.client.rebindMcp({ sessionId: "s1", expectedBindingHash: "hash" }),
        Code.FailedPrecondition,
      );
    } finally {
      await member.close();
      await admin.close();
    }
  });
  test("publication preserves schema and classification and maps stale CAS", async () => {
    const service = services();
    let calls = 0;
    service.validateSchemas = async (tools) => {
      expect(tools[0]!.classification).toBe("write");
    };
    service.repository.publish = async (_connectionId, _draftId, expected, input) => {
      calls++;
      if (expected !== 0) throw new Error("stale revision");
      const tools = reviewCatalog(input);
      return {
        id: "catalog-1",
        connectionId: row.id,
        kind: "published",
        revision: 1,
        configRevision: 1,
        authGeneration: 1,
        credentialIdentity: "account-a",
        tools,
        fingerprint: "catalog-hash",
        actorId: "owner",
        createdAt: new Date(0),
      };
    };
    const s = await spawn(service);
    const request = {
      connectionId: row.id,
      draftId: "draft-1",
      expectedRevision: 0,
      tools: [
        {
          upstreamName: "Create.Record",
          description: "Create a record",
          inputSchemaJson: '{"type":"object","additionalProperties":false}',
          classification: "write",
          enabled: true,
        },
      ],
    };
    try {
      const response = await s.client.publishMcp(request);
      expect(response.catalog?.tools[0]?.classification).toBe("write");
      expect(response.catalog?.tools[0]?.inputSchemaJson).toBe(request.tools[0]!.inputSchemaJson);
      await code(s.client.publishMcp({ ...request, expectedRevision: 1 }), Code.Aborted);
      await code(
        s.client.publishMcp({
          ...request,
          tools: [{ ...request.tools[0]!, inputSchemaJson: "not json" }],
        }),
        Code.InvalidArgument,
      );
      expect(calls).toBe(2);
    } finally {
      await s.close();
    }
  });
});

test("credential lifecycle requires a human administrator before service access", async () => {
  const member = await spawn(services(), "user");
  const serviceAccount = await spawn(services(), "admin", "apikey+agent@service.local");
  try {
    for (const s of [member, serviceAccount]) {
      await code(s.client.putMcpCredential({ value: "entered-value" }), Code.PermissionDenied);
      await code(s.client.authorizeMcp({ shareWithOrgSessions: true }), Code.PermissionDenied);
      await code(
        s.client.completeMcpAuthorization({ code: "provider-code", state: "provider-state" }),
        Code.PermissionDenied,
      );
      await code(s.client.disconnectMcp({}), Code.PermissionDenied);
    }
  } finally {
    await member.close();
    await serviceAccount.close();
  }
});
