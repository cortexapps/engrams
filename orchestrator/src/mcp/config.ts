import { isIP } from "node:net";
import { z } from "zod";

// Naming convention only. Coordinator reservation metadata protects the sealed secret.
const reference = z.string().regex(/^mcp[.-][a-zA-Z0-9_.-]{1,160}$/);
const configSchema = z
  .object({
    endpoint: z.string().max(2048),
    zone: z.string().regex(/^[a-z][a-z0-9-]{0,62}$/),
    auth: z.discriminatedUnion("kind", [
      z
        .object({ kind: z.literal("api_key"), secretRef: z.union([reference, z.literal("")]) })
        .strict(),
      z
        .object({
          kind: z.literal("oauth"),
          clientRegistrationId: z.string().min(1).max(128),
          shareWithOrgSessions: z.literal(true),
        })
        .strict(),
    ]),
  })
  .strict();

export type McpConfig = z.infer<typeof configSchema>;

/** This validates stored shape. The worker also enforces operator DNS/zone policy. */
export function validateMcpConfig(input: unknown): McpConfig {
  const config = configSchema.parse(input);
  const url = new URL(config.endpoint);
  if (
    url.protocol !== "https:" ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    isIP(url.hostname.replace(/^\[|\]$/g, "")) !== 0 ||
    url.href !== config.endpoint ||
    url.hostname.length > 253 ||
    !url.hostname.split(".").every((label) => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label))
  ) {
    throw new Error(
      "MCP endpoint must be a canonical HTTPS URL with a DNS hostname and no credentials, query, or fragment",
    );
  }
  return config;
}
