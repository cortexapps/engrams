/** OpenRouter as a model router. */

import { createOpenRouter } from "@openrouter/ai-sdk-provider";
import { z } from "zod";

import type { CatalogModel, ModelRouterSpec } from "../../model-routers/registry.ts";
import { makeDecisionsClient } from "./decisions.ts";

const OpenRouterModelSchema = z.object({
  id: z.string(),
  canonical_slug: z.string().optional(),
  name: z.string().optional(),
  description: z.string().nullish(),
  context_length: z.number().int().nonnegative().optional(),
  hugging_face_id: z.string().nullish(),
  supported_parameters: z.array(z.string()).optional(),
  pricing: z.object({ prompt: z.string().optional(), completion: z.string().optional() }).passthrough().optional(),
  architecture: z.object({ input_modalities: z.array(z.string()).optional(), output_modalities: z.array(z.string()).optional() }).passthrough().optional(),
}).passthrough();

const OpenRouterCatalogSchema = z.object({ data: z.array(OpenRouterModelSchema) });

/** Keeps the models a coding agent can use: text output, tool calls, not batch. */
export function parseOpenRouterCatalog(body: Uint8Array): CatalogModel[] {
  const decoded: unknown = JSON.parse(new TextDecoder().decode(body));
  const parsed = OpenRouterCatalogSchema.parse(decoded);
  const byId = new Map<string, CatalogModel>();
  for (const raw of parsed.data) {
    const output = raw.architecture?.output_modalities ?? [];
    const parameters = raw.supported_parameters ?? [];
    if (raw.id.endsWith(":batch") || !output.includes("text") || !parameters.includes("tools")) continue;
    byId.set(raw.id, {
      modelId: raw.id,
      canonicalSlug: raw.canonical_slug ?? raw.id,
      name: raw.name ?? raw.id,
      author: raw.id.split("/", 1)[0] ?? null,
      description: raw.description ?? null,
      contextLength: raw.context_length ?? 0,
      promptPrice: raw.pricing?.prompt ?? null,
      completionPrice: raw.pricing?.completion ?? null,
      inputModalities: raw.architecture?.input_modalities ?? [],
      outputModalities: output,
      supportedParameters: parameters,
      huggingFaceId: raw.hugging_face_id ?? null,
      upstream: raw as Record<string, unknown>,
    });
  }
  return [...byId.values()];
}

export const OPENROUTER: ModelRouterSpec = {
  id: "openrouter",
  label: "OpenRouter",
  description: "Shared access to tool-capable models through OpenRouter.",
  connector: "openrouter",
  protocols: {
    anthropic_messages: { basePath: "/api" },
    openai_responses: { basePath: "/api/v1" },
  },
  defaultModel: "deepseek/deepseek-v4-pro-0813",
  titleModel: "deepseek/deepseek-v4-flash",
  catalog: {
    path: "/api/v1/models/user?output_modalities=text&supported_parameters=tools&sort=newest",
    refreshIntervalMs: 6 * 60 * 60 * 1000,
    parse: parseOpenRouterCatalog,
  },
  // Model pages are on the public website, whatever the API region.
  modelUrl: (modelId) => `https://openrouter.ai/${modelId}`,
  chat: (apiKey, host) => {
    const provider = createOpenRouter({ apiKey, baseURL: `https://${host}/api/v1` });
    return (modelId) => provider.chat(modelId);
  },
  decisions: (apiKey, host) => makeDecisionsClient({ apiKey, host }),
};
