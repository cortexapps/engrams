/** A LiteLLM proxy as a model router.
 *
 * The proxy answers the Anthropic Messages API at `/v1/messages`, and the
 * OpenAI Responses, Chat Completions and model-list APIs under `/v1`, with
 * the proxy key as a bearer token. The admin sets the host; sessions reach
 * it over HTTPS on port 443. The admin also picks the model for each role,
 * because the proxy's models are the admin's own.
 */

import { createOpenAICompatible } from "@ai-sdk/openai-compatible";
import { z } from "zod";

import type { CatalogModel, ModelRouterSpec } from "../../model-routers/registry.ts";

const ModelListSchema = z.object({
  data: z.array(z.object({ id: z.string().min(1), owned_by: z.string().nullish() }).passthrough()),
});

/** The OpenAI model list carries an id and an owner only. */
export function parseLiteLlmCatalog(body: Uint8Array): CatalogModel[] {
  const parsed = ModelListSchema.parse(JSON.parse(new TextDecoder().decode(body)));
  const byId = new Map<string, CatalogModel>();
  for (const raw of parsed.data) {
    byId.set(raw.id, {
      modelId: raw.id,
      canonicalSlug: raw.id,
      name: raw.id,
      author: raw.owned_by ?? null,
      description: null,
      contextLength: 0,
      promptPrice: null,
      completionPrice: null,
      inputModalities: [],
      outputModalities: [],
      supportedParameters: [],
      huggingFaceId: null,
      upstream: raw as Record<string, unknown>,
    });
  }
  return [...byId.values()];
}

export const LITELLM: ModelRouterSpec = {
  id: "litellm",
  label: "LiteLLM",
  description: "The models behind your own LiteLLM proxy.",
  connector: "litellm",
  protocols: {
    anthropic_messages: { basePath: "" },
    openai_responses: { basePath: "/v1" },
  },
  models: {},
  catalog: {
    path: "/v1/models",
    refreshIntervalMs: 60 * 60 * 1000,
    parse: parseLiteLlmCatalog,
  },
  chat: (apiKey, host) => {
    const provider = createOpenAICompatible({
      name: "litellm",
      apiKey,
      baseURL: `https://${host}/v1`,
      supportsStructuredOutputs: true,
    });
    return (modelId) => provider.chatModel(modelId);
  },
};
