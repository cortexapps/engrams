import { z } from "zod";

import type { RouterModelInput } from "../db/model-routers.ts";

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

export function parseOpenRouterCatalog(body: Uint8Array): RouterModelInput[] {
  const decoded: unknown = JSON.parse(new TextDecoder().decode(body));
  const parsed = OpenRouterCatalogSchema.parse(decoded);
  const byId = new Map<string, RouterModelInput>();
  for (const raw of parsed.data) {
    const output = raw.architecture?.output_modalities ?? [];
    const parameters = raw.supported_parameters ?? [];
    if (raw.id.endsWith(":batch") || !output.includes("text") || !parameters.includes("tools")) continue;
    byId.set(raw.id, {
      routerId: "openrouter",
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
