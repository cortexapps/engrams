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

import type { ModelRouterStore, RouterSettings } from "../db/model-routers.ts";
import { registerModelRouters } from "../rpc/model-routers.ts";
import { ModelRouterService, RouterModelAudience } from "../gen/engram/app/v1/model_router_pb.ts";

function fakeStore(): ModelRouterStore {
  const settings = new Map<string, RouterSettings>();
  return {
    listModels: async () => [],
    getModel: async () => null,
    replaceCatalog: async () => ({ markedUnavailable: 0 }),
    updatePolicy: async () => null,
    getSyncState: async () => null,
    recordFailure: async () => {},
    getSettings: async (routerId) => settings.get(routerId) ?? null,
    setSettings: async (routerId, value) => {
      settings.set(routerId, value);
    },
  };
}

function client(role: "user" | "admin", secretNames: string[], host?: string) {
  const transport = createRouterTransport((router) =>
    registerModelRouters(router, {
      getSession: async () => ({ user: { id: "u1", role } }),
      store: fakeStore(),
      integration: { connectors: { list: async () => [] }, settingsFor: async (): Promise<Record<string, string>> => host ? { api_host: host } : {} },
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
    expect(result.routers.map((r) => r.id)).toEqual(["openrouter", "litellm"]);
    expect(result.routers[0].credentialConfigured).toBe(false);
    // No proxy host is set yet, so the router has no egress host.
    expect(result.routers[1].egressHosts).toEqual([]);
  });

  test("an admin's model per role replaces the built-in one; empty restores it", async () => {
    const api = client("admin", ["openrouter.api_key", "litellm.api_key"]);
    const [openrouter, litellm] = (await api.listModelRouters({})).routers;
    expect(openrouter.decisionModel).toBe("typesafe/jev-1.13");
    expect(litellm).toMatchObject({ defaultModel: "", titleModel: "", decisionModel: "" });

    const set = await api.updateModelRouterSettings({
      routerId: "litellm",
      defaultModel: "claude-sonnet",
      titleModel: " gpt-mini ",
      decisionModel: "",
    });
    expect(set.router).toMatchObject({ defaultModel: "claude-sonnet", titleModel: "gpt-mini", decisionModel: "" });

    await api.updateModelRouterSettings({ routerId: "openrouter", titleModel: "other/title" });
    const cleared = await api.updateModelRouterSettings({ routerId: "openrouter" });
    expect(cleared.router?.titleModel).toBe("deepseek/deepseek-v4-flash");
  });

  test("only an admin sets a router's models", async () => {
    const api = client("user", ["litellm.api_key"]);
    await expect(api.updateModelRouterSettings({ routerId: "litellm", defaultModel: "m" })).rejects.toThrow();
  });

  test("non-admin with the key: router listed AND credentialConfigured=true", async () => {
    const api = client("user", ["openrouter.api_key"]);
    const result = await api.listModelRouters({});
    expect(result.routers.map((r) => r.id)).toEqual(["openrouter"]);
    expect(result.routers[0].credentialConfigured).toBe(true);
    // Secret NAME stays admin-only.
    expect(result.routers[0].credentialSecret).toBe("");
  });

  test("reports the configured regional egress host", async () => {
    const result = await client("admin", ["openrouter.api_key"], "us.openrouter.ai").listModelRouters({});
    expect(result.routers[0].egressHosts).toEqual(["us.openrouter.ai"]);
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
