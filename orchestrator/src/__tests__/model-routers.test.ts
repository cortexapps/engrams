import { describe, expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";

import type { ModelRouterStore, RouterModelInput } from "../db/model-routers.ts";
import { refreshRouterCatalog } from "../model-routers/catalog.ts";
import { parseOpenRouterCatalog } from "../model-routers/openrouter.ts";
import { registerModelRouters } from "../rpc/model-routers.ts";
import {
  ModelRouterService,
  RouterModelAudience,
} from "../gen/engram/app/v1/model_router_pb.ts";

const response = (data: unknown, truncated = false) => ({
  status: 200,
  body: new TextEncoder().encode(JSON.stringify(data)),
  contentType: "application/json",
  truncated,
});

function model(id: string, overrides: Record<string, unknown> = {}) {
  return {
    id,
    canonical_slug: `${id}-canonical`,
    name: id,
    context_length: 128_000,
    supported_parameters: ["tools"],
    architecture: { input_modalities: ["text"], output_modalities: ["text"] },
    pricing: { prompt: "0.000001", completion: "0.000002" },
    ...overrides,
  };
}

function store() {
  const calls: { replacements: RouterModelInput[][]; failures: string[] } = {
    replacements: [],
    failures: [],
  };
  const value: ModelRouterStore = {
    listModels: async () => [],
    getModel: async () => null,
    replaceCatalog: async (_routerId, models) => {
      calls.replacements.push(models);
      return { markedUnavailable: 4 };
    },
    updatePolicy: async () => null,
    getSyncState: async () => null,
    recordFailure: async (_routerId, error) => { calls.failures.push(error); },
  };
  return { value, calls };
}

describe("OpenRouter model catalog", () => {
  test("keeps every text tool model and excludes batch or incompatible entries", () => {
    const parsed = parseOpenRouterCatalog(response({ data: [
      model("deepseek/deepseek-v4-pro-0813", { hugging_face_id: "deepseek-ai/DeepSeek-V4-Pro-0813", supported_parameters: ["tools", "reasoning_effort"] }),
      model("free/router:free"),
      model("provider/batch:batch"),
      model("provider/no-tools", { supported_parameters: ["temperature"] }),
      model("provider/image", { architecture: { output_modalities: ["image"] } }),
    ] }).body);
    expect(parsed.map((entry) => entry.modelId)).toEqual([
      "deepseek/deepseek-v4-pro-0813",
      "free/router:free",
    ]);
    expect(parsed[0]).toMatchObject({
      canonicalSlug: "deepseek/deepseek-v4-pro-0813-canonical",
      huggingFaceId: "deepseek-ai/DeepSeek-V4-Pro-0813",
    });
  });

  test("commits one complete refresh and reports unseen rows", async () => {
    const fake = store();
    const result = await refreshRouterCatalog("openrouter", {
      store: fake.value,
      fetch: async () => response({ data: [model("a/model"), model("b/model")] }),
      now: () => new Date("2026-08-13T00:00:00Z"),
    });
    expect(result).toEqual({ discovered: 2, available: 2, markedUnavailable: 4 });
    expect(fake.calls.replacements).toHaveLength(1);
    expect(fake.calls.failures).toEqual([]);
  });

  test("retains the prior cache after a truncated or empty refresh", async () => {
    for (const result of [response({ data: [model("a/model")] }, true), response({ data: [] })]) {
      const fake = store();
      await expect(refreshRouterCatalog("openrouter", {
        store: fake.value,
        fetch: async () => result,
      })).rejects.toThrow();
      expect(fake.calls.replacements).toEqual([]);
      expect(fake.calls.failures).toHaveLength(1);
    }
  });
});

describe("ModelRouterService policy audiences", () => {
  const row = {
    routerId: "openrouter",
    modelId: "deepseek/deepseek-v4-pro-0813",
    canonicalSlug: "deepseek/deepseek-v4-pro-20260813",
    name: "DeepSeek V4 Pro 0813",
    author: "deepseek",
    description: null,
    contextLength: 163_840,
    promptPrice: "0.0000002",
    completionPrice: "0.0000008",
    inputModalities: ["text"],
    outputModalities: ["text"],
    supportedParameters: ["tools", "reasoning_effort"],
    huggingFaceId: null,
    upstream: {},
    available: true,
    enabled: true,
    userEnabled: true,
    updatedAt: new Date("2026-08-13T00:00:00Z"),
  };

  function client(role: "user" | "admin") {
    const audiences: string[] = [];
    const fake = store().value;
    fake.listModels = async (_routerId, audience) => {
      audiences.push(audience);
      return [row];
    };
    fake.getSyncState = async () => ({
      routerId: "openrouter",
      lastSuccessfulSyncAt: null,
      lastAttemptAt: null,
      lastError: null,
    });
    fake.updatePolicy = async (_routerId, _modelId, enabled, userEnabled) => ({
      ...row,
      enabled,
      userEnabled,
    });
    const transport = createRouterTransport((router) =>
      registerModelRouters(router, {
        getSession: async () => ({ user: { id: "u1", role } }),
        store: fake,
        orgSecret: { listSecrets: async () => ({ secrets: [{ name: "openrouter.api_key" }] }) },
      }),
    );
    return { api: createClient(ModelRouterService, transport), audiences };
  }

  test("human callers can list only the user audience", async () => {
    const { api, audiences } = client("user");
    const result = await api.listRouterModels({
      routerId: "openrouter",
      audience: RouterModelAudience.USER,
    });
    expect(result.models[0]).toMatchObject({
      id: "deepseek/deepseek-v4-pro-0813",
      supportsReasoning: true,
    });
    expect(audiences).toEqual(["user"]);
    await expect(
      api.listRouterModels({
        routerId: "openrouter",
        audience: RouterModelAudience.PROGRAMMATIC,
      }),
    ).rejects.toMatchObject({ code: Code.PermissionDenied });
  });

  test("admin policy rejects user-enabled without programmatic enablement", async () => {
    const { api } = client("admin");
    try {
      await api.updateRouterModelPolicy({
        routerId: "openrouter",
        modelId: row.modelId,
        enabled: false,
        userEnabled: true,
      });
      throw new Error("expected policy rejection");
    } catch (error) {
      expect(error).toBeInstanceOf(ConnectError);
      expect((error as ConnectError).code).toBe(Code.InvalidArgument);
    }
  });
});
