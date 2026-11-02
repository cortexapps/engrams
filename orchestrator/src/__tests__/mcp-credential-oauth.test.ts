import { expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";
import { create } from "@bufbuild/protobuf";
import { checkDb } from "../db/client.ts";
import { makeIntegrationConnectionStore } from "../db/integration-connections.ts";
import { makeCredentialMetadataStore } from "../mcp/credential-store.ts";
import { makeCredentialAdmin } from "../mcp/credential-admin.ts";
import {
  CredentialControl,
  CredentialKind,
  CredentialStatusSchema,
  type AuthorityContext,
  type CredentialStatus,
} from "../gen/engram/mcp/v1/control_pb.ts";
const reachable = process.env.ORCHESTRATOR_DATABASE_URL ? await checkDb() : false;
const config = {
  endpoint: "https://tools.example.test/mcp",
  zone: "internal",
  auth: {
    kind: "oauth" as const,
    clientRegistrationId: "public-client",
    shareWithOrgSessions: true as const,
  },
};
async function fixture() {
  const connections = makeIntegrationConnectionStore(),
    store = makeCredentialMetadataStore();
  const row = await connections.create({
    alias: `mcp-${crypto.randomUUID()}`,
    displayName: "Tools",
    provider: "mcp",
    config,
  });
  let clock = Date.now(),
    flowContext: AuthorityContext | undefined,
    current: CredentialStatus | undefined,
    lostCallback = false,
    outage = false;
  const calls: string[] = [];
  const control = createClient(
    CredentialControl,
    createRouterTransport((router) =>
      router.service(CredentialControl, {
        async getStatus() {
          if (outage) throw new ConnectError("outage", Code.Unavailable);
          if (!current) throw new ConnectError("absent", Code.NotFound);
          return current;
        },
        async beginAuthorization(req) {
          calls.push("begin");
          expect(req.sharingConsent).toBe(true);
          expect(req.clientSecret).toBeUndefined();
          expect(req.registrationId).toBe("public-client");
          expect(req.context!.resource!.zoneId).toBe("internal");
          return {
            protocol: 1,
            flowId: "flow",
            context: {
              connectionId: row.id,
              configRevision: req.context!.configRevision,
              authorizationGeneration: req.context!.previousAuthorizationGeneration + 1n,
              credentialIdentity: "ceremony",
              resource: req.context!.resource,
              policyDigest: "a".repeat(64),
              policyRevision: "v1",
            },
            authorizationUrl: "https://auth.example.test/authorize?state=secret-state",
            expiresAt: BigInt(clock + 600000),
          };
        },
        async completeAuthorization(req) {
          calls.push("complete");
          expect(req.state).toBe("secret-state");
          expect(req.code).toBe("secret-code");
          flowContext = req.context!;
          current = create(CredentialStatusSchema, {
            protocol: 1,
            context: flowContext,
            reference: { kind: CredentialKind.OAUTH, registrationId: "public-client" },
            version: (current?.version ?? 0n) + 1n,
            status: "connected",
          });
          if (lostCallback) throw new ConnectError("lost reply", Code.NotFound);
          return current;
        },
        async revoke(req) {
          calls.push("revoke");
          expect(req.expected!.version).toBe(current!.version);
          current = create(CredentialStatusSchema, {
            ...current!,
            version: current!.version + 1n,
            status: "revoked",
          });
          return current;
        },
      }),
    ),
  );
  const admin = makeCredentialAdmin({
    store,
    control,
    now: () => new Date(clock),
    registrations: [
      {
        id: "public-client",
        clientId: "client-id",
        confidential: false,
        redirectUri: "https://app.example.test/oauth/mcp",
        issuer: "https://auth.example.test/",
        resource: { zoneId: "internal", endpoint: config.endpoint },
        scopes: ["tools"],
      },
    ],
  });
  const callback = {
    flowId: "flow",
    state: "secret-state",
    code: "secret-code",
    issuer: "https://auth.example.test/",
    redirectUri: "https://app.example.test/oauth/mcp",
  };
  return {
    connections,
    store,
    row,
    admin,
    callback,
    calls,
    lose() {
      lostCallback = true;
    },
    advance() {
      clock += 35000;
    },
    outage(value: boolean) {
      outage = value;
    },
    current: () => current,
    async cleanup() {
      await connections.delete(row.id);
    },
  };
}
test.skipIf(!reachable)(
  "OAuth admin consent, exact callback actor and redirect, one-use publication and metadata-only persistence",
  async () => {
    const f = await fixture();
    try {
      await expect(f.admin.begin(f.row.id, 1, 1, "admin", false)).rejects.toThrow("consent");
      expect(f.calls).toEqual([]);
      await f.admin.begin(f.row.id, 1, 1, "admin", true);
      let row = await f.store.get(f.row.id);
      expect(JSON.stringify(row)).not.toContain("secret-state");
      await expect(f.admin.complete(row.id, 2, 2, "other", f.callback)).rejects.toThrow("stale");
      await expect(
        f.admin.complete(row.id, 2, 2, "admin", {
          ...f.callback,
          redirectUri: "https://evil.example/",
        }),
      ).rejects.toThrow("callback");
      await f.admin.complete(row.id, 2, 2, "admin", f.callback);
      row = await f.store.get(row.id);
      expect(row.revoked).toBe(false);
      expect(row.authGeneration).toBe(3);
      expect(row.configRevision).toBe(2);
      expect(JSON.stringify(row)).not.toContain("secret-code");
      expect(JSON.stringify(row)).not.toContain("secret-state");
      await expect(f.admin.complete(row.id, 2, 3, "admin", f.callback)).rejects.toThrow("stale");
      expect(f.calls).toEqual(["begin", "complete"]);
    } finally {
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "switch during pending OAuth blocks the old callback before exchange",
  async () => {
    const f = await fixture();
    try {
      await f.admin.begin(f.row.id, 1, 1, "admin", true);
      await f.admin.configure(f.row.id, 2, 2, "admin", {
        endpoint: config.endpoint,
        zone: "internal",
        auth: { kind: "api_key", secretRef: "" },
      });
      await expect(f.admin.complete(f.row.id, 2, 2, "admin", f.callback)).rejects.toThrow();
      expect(f.calls).toEqual(["begin"]);
    } finally {
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "lost callback requires recovery that revokes the published coordinator authority",
  async () => {
    const f = await fixture();
    try {
      await f.admin.begin(f.row.id, 1, 1, "admin", true);
      f.lose();
      await expect(f.admin.complete(f.row.id, 2, 2, "admin", f.callback)).rejects.toThrow();
      expect(f.current()!.status).toBe("connected");
      f.advance();
      await f.admin.recover(f.row.id, 2, 2, "admin");
      expect(f.current()!.status).toBe("revoked");
      expect((await f.store.get(f.row.id)).revoked).toBe(true);
      expect(f.calls).toEqual(["begin", "complete", "revoke"]);
    } finally {
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "recovery fences a lost reconnect callback over a previously revoked receipt",
  async () => {
    const f = await fixture();
    try {
      await f.admin.begin(f.row.id, 1, 1, "admin", true);
      await f.admin.complete(f.row.id, 2, 2, "admin", f.callback);
      await f.admin.begin(f.row.id, 2, 3, "admin", true);
      f.lose();
      await expect(f.admin.complete(f.row.id, 3, 4, "admin", f.callback)).rejects.toThrow();
      f.advance();
      await f.admin.recover(f.row.id, 3, 4, "admin");
      expect(f.current()!.status).toBe("revoked");
    } finally {
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "recovery keeps its durable fence on outage and clears only confirmed absence",
  async () => {
    const f = await fixture();
    try {
      const now = new Date();
      await f.store.claim(f.row.id, 1, 1, "admin", config, now);
      f.advance();
      f.advance();
      f.outage(true);
      await expect(f.admin.recover(f.row.id, 2, 2, "admin")).rejects.toThrow();
      expect((await f.store.get(f.row.id)).mcpCredential.transition).toBeDefined();
      f.outage(false);
      f.advance();
      await f.admin.recover(f.row.id, 2, 2, "admin");
      const row = await f.store.get(f.row.id);
      expect(row.revoked).toBe(true);
      expect(row.mcpCredential.transition).toBeUndefined();
    } finally {
      await f.cleanup();
    }
  },
);
