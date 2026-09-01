/**
 * A model router without its credential org secret must be invisible
 * everywhere outside the admin settings panel (found on the first
 * fresh-deployment walk: OpenRouter surfaces assumed the key existed).
 *
 *  - non-admins never see the router in listModelRouters and cannot
 *    browse its catalog;
 *  - admins still see it (the settings panel is where the key is
 *    saved) with credentialConfigured=false, but cannot refresh its
 *    catalog until the key exists;
 *  - once the secret exists, non-admins see the router WITH
 *    credentialConfigured=true (it was previously always false for
 *    them, which would defeat the client-side picker filter).
 */

import { describe, expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";

import type { ModelRouterStore } from "../db/model-routers.ts";
import { registerModelRouters } from "../rpc/model-routers.ts";
import { ModelRouterService, RouterModelAudience } from "../gen/engram/app/v1/model_router_pb.ts";

function fakeStore(): ModelRouterStore {
  return {
    listModels: async () => [],
    getModel: async () => null,
    replaceCatalog: async () => ({ markedUnavailable: 0 }),
    updatePolicy: async () => null,
    getSyncState: async () => null,
    recordFailure: async () => {},
  };
}

function client(role: "user" | "admin", secretNames: string[]) {
  const transport = createRouterTransport((router) =>
    registerModelRouters(router, {
      getSession: async () => ({ user: { id: "u1", role } }),
      store: fakeStore(),
      orgSecret: {
        listSecrets: async () => ({ secrets: secretNames.map((name) => ({ name })) }),
      },
    }),
  );
  return createClient(ModelRouterService, transport);
}

describe("router connection gating", () => {
  test("non-admin with no key: no routers listed", async () => {
    const api = client("user", []);
    const result = await api.listModelRouters({});
    expect(result.routers).toHaveLength(0);
  });

  test("admin with no key: router listed as not configured (the setup surface)", async () => {
    const api = client("admin", []);
    const result = await api.listModelRouters({});
    expect(result.routers.map((r) => r.id)).toEqual(["openrouter"]);
    expect(result.routers[0].credentialConfigured).toBe(false);
  });

  test("non-admin with the key: router listed AND credentialConfigured=true", async () => {
    const api = client("user", ["openrouter.api_key"]);
    const result = await api.listModelRouters({});
    expect(result.routers.map((r) => r.id)).toEqual(["openrouter"]);
    expect(result.routers[0].credentialConfigured).toBe(true);
    // Secret NAME stays admin-only.
    expect(result.routers[0].credentialSecret).toBe("");
  });

  test("non-admin with no key: the catalog is not browsable", async () => {
    const api = client("user", []);
    await expect(
      api.listRouterModels({ routerId: "openrouter", audience: RouterModelAudience.USER }),
    ).rejects.toMatchObject({ code: Code.NotFound } satisfies Partial<ConnectError>);
  });

  test("admin with no key: manual refresh is refused with a pointer to setup", async () => {
    const api = client("admin", []);
    await expect(
      api.refreshRouterModels({ routerId: "openrouter" }),
    ).rejects.toMatchObject({ code: Code.FailedPrecondition } satisfies Partial<ConnectError>);
  });
});
