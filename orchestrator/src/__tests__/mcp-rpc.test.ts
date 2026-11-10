import { describe, expect, spyOn, test } from "bun:test";
import { createClient, createRouterTransport, Code, ConnectError } from "@connectrpc/connect";
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
import { MCP_LIMITS } from "../mcp/limits.ts";

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
      setEnabled: unavailable,
      saveDraft: unavailable,
      publish: unavailable,
      getCatalog: unavailable,
      getLatestDraft: unavailable,
    },
    journal: {
      get: unavailable,
      readSession: unavailable,
      readExecutionResult: unavailable,
      getAdmission: unavailable,
      hasUnknown: unavailable,
      assertCurrent: unavailable,
      create: unavailable,
      requestApproval: unavailable,
      pending: unavailable,
      decide: unavailable,
      admit: unavailable,
      admitSchema: unavailable,
      finishSchema: unavailable,
      rejectBeforeDispatch: unavailable,
      assertDispatchCurrent: unavailable,
      dispatch: unavailable,
      finish: unavailable,
      cancel: unavailable,
      cancelInternal: unavailable,
      cancellationReceipt: unavailable,
      resolveUnknown: unavailable,
      expire: unavailable,
      expireUnadmitted: unavailable,
      expiryScopes: unavailable,
    },
  };
}
async function spawn(service?: McpServices, role = "admin", email?: string, connection = row) {
  const server = buildServer(new Hono(), (router) =>
    router.service(
      IntegrationService,
      mcpHandlers({
        services: service,
        connections: {
          ...connections,
          get: async (id) => (id === connection.id ? connection : null),
        },
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
      await code(s.client.getMcpExecutionResult({}), Code.FailedPrecondition);
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
  test("setup reports only a registered enabled discovery hook", async () => {
    const service = services();
    const admin = await spawn(service);
    try {
      expect((await admin.client.getMcpSetup({})).discoveryAvailable).toBe(false);
      service.discovery = { discover: unavailable };
      expect((await admin.client.getMcpSetup({})).discoveryAvailable).toBe(true);
      service.enabled = false;
      const disabled = await admin.client.getMcpSetup({});
      expect(disabled.discoveryAvailable).toBe(false);
      expect(disabled.recoveryAvailable).toBe(false);
      await code(admin.client.recoverMcpCredential({}), Code.FailedPrecondition);
      await code(admin.client.disconnectMcp({}), Code.FailedPrecondition);
    } finally {
      await admin.close();
    }
  });
  test("catalog read requires an admin and returns the persisted draft and current publication", async () => {
    const service = services();
    const base = {
      connectionId: row.id,
      configRevision: 1,
      authGeneration: 1,
      credentialIdentity: "account-a",
      actorId: "owner",
      createdAt: new Date(0),
      fingerprint: "hash",
    };
    service.repository.getLatestDraft = async () => ({
      ...base,
      id: "draft",
      kind: "draft",
      revision: 0,
      tools: [
        {
          upstreamName: "read.item",
          description: "<script>untrusted</script>",
          inputSchema: { type: "object" },
        },
      ],
    });
    service.repository.getCatalog = async (_id, revision) => ({
      ...base,
      id: "published",
      kind: "published",
      revision,
      tools: [
        {
          upstreamName: "read.item",
          description: "old",
          inputSchema: { type: "object" },
          classification: "read",
          enabled: true,
          fingerprint: "tool",
        },
      ],
    });
    const current = { ...row, mcp: { ...row.mcp!, catalogRevision: 2 } };
    const admin = await spawn(service, "admin", undefined, current);
    const member = await spawn(service, "user");
    const serviceAccount = await spawn(service, "admin", "apikey+agent@service.local");
    try {
      await code(
        serviceAccount.client.getMcpCatalogs({ connectionId: row.id }),
        Code.PermissionDenied,
      );
      await code(member.client.getMcpCatalogs({ connectionId: row.id }), Code.PermissionDenied);
      const result = await admin.client.getMcpCatalogs({ connectionId: row.id });
      expect(result.draft?.tools[0]?.classification).toBe("");
      expect(result.draft?.tools[0]?.description).toBe("<script>untrusted</script>");
      expect(result.published?.revision).toBe(2);
      expect(result.published?.tools[0]?.classification).toBe("read");
      expect(result.catalogRevision).toBe(2);
      service.enabled = false;
      expect(
        (await admin.client.getMcpCatalogs({ connectionId: row.id })).published?.revision,
      ).toBe(2);
      service.repository.getLatestDraft = async () => ({
        ...base,
        id: "oversized",
        kind: "draft",
        revision: 0,
        tools: Array.from({ length: 129 }, (_, i) => ({
          upstreamName: `tool-${i}`,
          description: "",
          inputSchema: { type: "object" },
        })),
      });
      await code(admin.client.getMcpCatalogs({ connectionId: row.id }), Code.FailedPrecondition);
    } finally {
      await member.close();
      await serviceAccount.close();
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
          inputSchemaJson:
            '{"type":"object","properties":{"count":{"type":"number","minimum":0.1,"maximum":1e3}}}',
          outputSchemaJson:
            '{"type":"object","properties":{"count":{"type":"number","minimum":0.1,"maximum":1000}}}',
          classification: "write",
          enabled: true,
        },
      ],
    };
    try {
      const response = await s.client.publishMcp(request);
      expect(response.catalog?.tools[0]?.classification).toBe("write");
      expect(response.catalog!.tools[0]!.inputSchemaJson).toBe(request.tools[0]!.outputSchemaJson);
      expect(response.catalog!.tools[0]!.inputSchemaJson).toBe(
        response.catalog!.tools[0]!.outputSchemaJson,
      );
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
  for (const field of ["inputSchemaJson", "outputSchemaJson"] as const) {
    test.each([
      [
        "rounded decimal",
        '{"type":"object","properties":{"count":{"minimum":0.10000000000000001}}}',
      ],
      ["unsafe integer", '{"type":"object","properties":{"count":{"maximum":9007199254740993}}}'],
      ["raw byte limit", '{"type":"object"}'.padStart(MCP_LIMITS.argumentBytes + 1)],
    ])(
      `publication rejects ${field} with %s before validation or persistence`,
      async (_name, raw) => {
        const service = services();
        let validations = 0;
        let publications = 0;
        service.validateSchemas = async () => {
          validations++;
        };
        service.repository.publish = async () => {
          publications++;
          return unavailable();
        };
        const s = await spawn(service);
        try {
          await code(
            s.client.publishMcp({
              connectionId: row.id,
              draftId: "draft-1",
              tools: [
                {
                  upstreamName: "Create.Record",
                  description: "Create a record",
                  inputSchemaJson: '{"type":"object"}',
                  [field]: raw,
                  classification: "write",
                  enabled: true,
                },
              ],
            }),
            Code.InvalidArgument,
          );
          expect(validations).toBe(0);
          expect(publications).toBe(0);
        } finally {
          await s.close();
        }
      },
    );
  }
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

test("actual MCP setup projects exact configured resources through fresh publication and switches", async () => {
  const { drizzle } = await import("drizzle-orm/node-postgres");
  const schema = await import("../db/schema.ts");
  const { ZonePolicy } = await import("../../../mcp-worker/src/network.ts");
  const { createHash } = await import("node:crypto");
  const { canonicalJson } = await import("../mcp/hash.ts");
  const { SessionPolicyService } = await import("../gen/engram/mcp/v1/session_policy_pb.ts");
  const controlClients = await import("../mcp/control-client.ts");
  const { makeMcpRuntime } = await import("../mcp/runtime.ts");
  const hash = (value: string) => `sha256:${createHash("sha256").update(value).digest("hex")}`;
  const infrastructure = {
    deploymentIdentity: hash("deployment"),
    networkIdentity: hash("network"),
    trustIdentity: hash("trust"),
    caContentDigests: { control: hash("ca") },
  };
  let enabled = true,
    available = true,
    admissionOpen = true,
    pending = false,
    storeFailed = false,
    generation = 1;
  let authorities = ["api.example.test", "issuer.example.test", "tools.example.test"];
  const control = createClient(
    SessionPolicyService,
    createRouterTransport((router) => {
      router.service(SessionPolicyService, {
        readPublishedOperator: async () => {
          if (!available) throw new ConnectError("publication unavailable", Code.Unavailable);
          const zone = {
            id: "public",
            revision: String(generation),
            mode: "public" as const,
            authorities,
            aliases: [],
            cidrs: [],
            denyCidrs: [],
          };
          const candidate = {
            version: 1,
            zones: {
              public: {
                revision: zone.revision,
                worker_digest: `sha256:${new ZonePolicy(zone).digest}`,
                authorities,
                aliases: [],
                configuration: JSON.parse(canonicalJson(zone)),
              },
            },
            deployment_identity: infrastructure.deploymentIdentity,
            network_identity: infrastructure.networkIdentity,
            trust_identity: infrastructure.trustIdentity,
            ca_content_digests: infrastructure.caContentDigests,
          };
          return {
            protocol: 1,
            publicationJson: new TextEncoder().encode(
              JSON.stringify({
                published: {
                  generation,
                  expected_generation: generation === 1 ? null : generation - 1,
                  identity: hash(JSON.stringify(candidate)),
                  candidate,
                },
                pending: pending ? {} : null,
                admission_open: admissionOpen,
              }),
            ),
          };
        },
      });
    }),
  );
  const transport = spyOn(controlClients, "makeSessionPolicyControlClient").mockReturnValue(
    control,
  );
  const tls = {
    url: "https://control.example.test/",
    ca: "ca",
    cert: "cert",
    key: "key",
    serverFingerprint: Array(32).fill("AA").join(":"),
  };
  const oauth = { zoneId: "public", endpoint: "https://tools.example.test/connectors/custom/rpc" };
  const api = { zoneId: "public", endpoint: "https://api.example.test/service/tools/v2" };
  const configuration = {
    infrastructure,
    control: tls,
    workers: new Map([["public", { enabled: true, deploymentId: "workers", tls }]]),
    registrations: [
      {
        id: "oauth",
        clientId: "client",
        confidential: false,
        redirectUri: "https://app.example.test/integrations/mcp/callback",
        issuer: "https://issuer.example.test",
        resource: oauth,
        scopes: [],
      },
    ],
    destinations: [
      api,
      api,
      { zoneId: "public", endpoint: "https://tools.example.test/path?query=invalid" },
      { zoneId: "public", endpoint: "https://unpublished.example.test/rpc" },
    ],
  };
  const { create } = await import("@bufbuild/protobuf");
  const { CredentialStatusSchema, CredentialKind } =
    await import("../gen/engram/mcp/v1/control_pb.ts");
  const credentialStore = await import("../mcp/credential-store.ts");
  const db = drizzle.mock({ schema });
  const status = create(CredentialStatusSchema, {
    protocol: 1,
    status: "connected",
    version: 1n,
    context: { credentialIdentity: "account-a" },
    reference: { kind: CredentialKind.API_KEY, name: "mcp.tools", connectionId: row.id },
  });
  const metadata = credentialStore.makeCredentialMetadataStore(db);
  // Only store I/O and the physical credential transport are replaced.
  const storeSpy = spyOn(credentialStore, "makeCredentialMetadataStore").mockReturnValue({
    ...metadata,
    get: async () => {
      if (storeFailed) throw Error("credential metadata unavailable");
      return {
        ...row,
        configRevision: 1,
        authGeneration: 1,
        credentialIdentity: "account-a",
        catalogRevision: 0,
        revoked: false,
        mcpCredential: { status: credentialStore.statusMetadata(status) },
      };
    },
  });
  const credentialControl = controlClients.makeCredentialControlClient(tls);
  const credentialSpy = spyOn(controlClients, "makeCredentialControlClient").mockReturnValue({
    ...credentialControl,
    getStatus: async () => status,
  });
  const runtime = makeMcpRuntime({
    db,
    deploymentId: "app",
    enabled: () => enabled,
    configuration,
    bridge: (client) => client,
  });
  const app = await spawn(runtime.services);
  try {
    const setup = await app.client.getMcpSetup({});
    expect(setup.enabled).toBe(true);
    expect(setup.registrations).toHaveLength(1);
    expect(setup.destinations.map(({ zoneId, endpoint }) => ({ zoneId, endpoint }))).toEqual([
      api,
      oauth,
    ]);
    // An API-key-only deployment has no OAuth resource metadata.
    const apiOnly = makeMcpRuntime({
      db: drizzle.mock({ schema }),
      deploymentId: "app",
      enabled: () => enabled,
      configuration: { ...configuration, registrations: [], destinations: [api] },
      bridge: (client) => client,
    });
    const apiApp = await spawn(apiOnly.services);
    try {
      const setup = await apiApp.client.getMcpSetup({});
      expect(setup.registrations).toEqual([]);
      expect(setup.destinations.map(({ zoneId, endpoint }) => ({ zoneId, endpoint }))).toEqual([
        api,
      ]);
    } finally {
      await apiApp.close();
    }
    authorities = ["issuer.example.test", "tools.example.test"];
    generation++;
    expect((await app.client.getMcpSetup({})).destinations.map((d) => d.endpoint)).toEqual([
      oauth.endpoint,
    ]);
    authorities = ["issuer.example.test"];
    generation++;
    expect((await app.client.getMcpSetup({})).destinations).toEqual([]);
    authorities = ["api.example.test", "issuer.example.test", "tools.example.test"];
    generation++;
    configuration.workers.get("public")!.enabled = false;
    expect((await app.client.getMcpSetup({})).destinations).toEqual([]);
    configuration.workers.get("public")!.enabled = true;
    enabled = false;
    expect((await app.client.getMcpSetup({})).destinations).toEqual([]);
    enabled = true;
    for (const failure of ["unavailable", "pending", "closed"]) {
      available = failure !== "unavailable";
      admissionOpen = failure === "unavailable";
      pending = failure === "pending";
      const setup = await app.client.getMcpSetup({});
      expect(setup.destinations).toEqual([]);
      expect(setup.recoveryAvailable).toBe(true);
      const current = await app.client.getMcpSetup({ connectionId: row.id });
      expect(current.current?.connection?.id).toBe(row.id);
      expect(current.current?.credential?.status).toBe("connected");
      expect(current.current?.credential?.reference).toBe("mcp.tools");
      expect(current.current?.credential?.credentialIdentity).toBe("account-a");
      expect(current.destinations).toEqual([]);
      if (!runtime.operator) throw Error("configured operator missing");
      await expect(
        runtime.operator.select({
          endpoint: api.endpoint,
          zone: api.zoneId,
          auth: { kind: "api_key", secretRef: "" },
        }),
      ).rejects.toThrow();
    }
    storeFailed = true;
    await code(app.client.getMcpSetup({ connectionId: row.id }), Code.FailedPrecondition);
  } finally {
    await app.close();
    transport.mockRestore();
    credentialSpy.mockRestore();
    storeSpy.mockRestore();
  }
});
