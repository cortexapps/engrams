import { afterEach, expect, spyOn, test } from "bun:test";
import { create } from "@bufbuild/protobuf";
import type { OpenRouterProvider } from "@openrouter/ai-sdk-provider";
import type { DecisionsClient } from "../integrations/openrouter-decisions.ts";
import "../integrations/openrouter.ts";
import { getIntegrationClient, invalidateIntegrationClient } from "../integrations/clients.ts";
import { runIntegrationOp, type RunOpDeps } from "../integrations/run-op.ts";
import { compileIntegrationPolicy, connectorRegistry } from "../connectors/registry.ts";
import {
  ResolveIntegrationCredentialResponseSchema,
  RunIntegrationOpResponseSchema,
} from "../gen/engram/app/v1/integration_op_pb.ts";

afterEach(() => invalidateIntegrationClient("openrouter"));

test("the default and regional settings control connector injection and catalog requests", async () => {
  for (const configured of [undefined, "us.openrouter.ai", "eu.openrouter.ai"]) {
    const host = configured ?? "openrouter.ai";
    const settings: Record<string, string> = configured ? { api_host: configured } : {};
    const policy = compileIntegrationPolicy(
      [{ connectionId: "default-openrouter", provider: "openrouter", operation: "chat:write", resourceConstraints: [], settings }],
      connectorRegistry(),
    );
    expect(policy.network.allow_hosts).toEqual([host]);
    expect(policy).toMatchObject({ injects: [{ hosts: [host] }] });

    await runIntegrationOp("openrouter", { method: "GET", path: "/api/v1/models/user" }, {
      connectors: { list: async () => [] },
      settingsFor: async () => settings,
      integrationOp: {
        runIntegrationOp: async (req) => {
          expect(req.host).toBe(host);
          expect(req.path).toBe("/api/v1/models/user");
          return create(RunIntegrationOpResponseSchema);
        },
        resolveIntegrationCredential: async () => { throw new Error("unused"); },
      },
    });
  }
});

test("SDK and Decisions requests follow a region change while the credential is cached", async () => {
  let host: string | undefined;
  const urls: string[] = [];
  const fetchSpy = spyOn(globalThis, "fetch").mockImplementation(Object.assign(async (input: URL | RequestInfo) => {
    urls.push(input instanceof Request ? input.url : String(input));
    return Response.json({
      id: "reply",
      created: 1,
      model: "test-model",
      choices: [{ index: 0, message: { role: "assistant", content: "hello" }, finish_reason: "stop" }],
      answers: {},
      usage: { total_tokens: 2, prompt_tokens: 1, completion_tokens: 1, input_tokens: 1, output_tokens: 1 },
    });
  }, { preconnect: globalThis.fetch.preconnect }));
  const deps: RunOpDeps = {
    connectors: { list: async () => [] },
    settingsFor: async (): Promise<Record<string, string>> => host ? { api_host: host } : {},
    integrationOp: {
      runIntegrationOp: async () => { throw new Error("unused"); },
      resolveIntegrationCredential: async () => create(ResolveIntegrationCredentialResponseSchema, {
        credential: { cred: { case: "bearer", value: { token: "test-key" } } },
      }),
    },
  };
  try {
    for (const configured of [undefined, "us.openrouter.ai", "eu.openrouter.ai"]) {
      host = configured;
      const clients = await getIntegrationClient<{ sdk: OpenRouterProvider; decisions: DecisionsClient }>("openrouter", deps);
      await clients.sdk.chat("test-model").doGenerate({
        prompt: [{ role: "user", content: [{ type: "text", text: "hello" }] }],
      });
      await clients.decisions.decide({ model: "test-model", state: "hello", questions: {} });
      expect(urls.splice(0)).toEqual([
        `https://${host ?? "openrouter.ai"}/api/v1/chat/completions`,
        `https://${host ?? "openrouter.ai"}/api/alpha/decisions`,
      ]);
    }
  } finally {
    fetchSpy.mockRestore();
  }
});
