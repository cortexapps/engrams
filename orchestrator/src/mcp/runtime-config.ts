import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { z } from "zod";
import type { CredentialControlTls } from "./control-client.ts";
import type { McpRegistrationMetadata } from "./credential-admin.ts";
import type { OperatorInfrastructure } from "./operator.ts";

const hash = z.string().regex(/^sha256:[0-9a-f]{64}$/);
const tlsSchema = z
  .object({
    url: z.string().url(),
    caName: z.string().min(1),
    certFile: z.string().min(1),
    keyFile: z.string().min(1),
    serverFingerprint: z.string().regex(/^(?:[0-9A-F]{2}:){31}[0-9A-F]{2}$/),
  })
  .strict();
const configSchema = z
  .object({
    version: z.literal(1),
    infrastructure: z
      .object({ deploymentIdentity: hash, networkIdentity: hash, trustIdentity: hash })
      .strict(),
    caFiles: z.record(z.string().min(1).max(128), z.string().min(1)),
    control: tlsSchema,
    workers: z
      .array(
        z
          .object({
            zoneId: z.string().min(1).max(128),
            deploymentId: z.string().min(1).max(256),
            enabled: z.boolean().default(false),
            tls: tlsSchema,
          })
          .strict(),
      )
      .min(1)
      .max(128),
    registrations: z
      .array(
        z
          .object({
            id: z.string().min(1),
            clientId: z.string().min(1),
            confidential: z.boolean(),
            redirectUri: z.string().url(),
            issuer: z.string().url(),
            resource: z.object({ zoneId: z.string().min(1), endpoint: z.string().url() }).strict(),
            scopes: z.array(z.string()),
          })
          .strict(),
      )
      .max(128)
      .default([]),
  })
  .strict();
export interface McpRuntimeConfiguration {
  infrastructure: OperatorInfrastructure;
  control: CredentialControlTls;
  workers: Map<string, { enabled: boolean; deploymentId: string; tls: CredentialControlTls }>;
  registrations: McpRegistrationMetadata[];
}
/** Only operator-owned files supply transport configuration. Published coordinator
 * authority must independently match it before a worker is selected. */
export async function loadMcpRuntimeConfiguration(path: string): Promise<McpRuntimeConfiguration> {
  const raw = await readFile(path);
  if (raw.length > 2 * 1024 * 1024) throw Error("MCP runtime configuration exceeds limit");
  const config = configSchema.parse(JSON.parse(raw.toString("utf8")));
  const ca = new Map<string, string>();
  const caContentDigests: Record<string, string> = {};
  for (const [name, file] of Object.entries(config.caFiles)) {
    const bytes = await readFile(file);
    if (!bytes.length || bytes.length > 1024 * 1024) throw Error("MCP trust roots unavailable");
    ca.set(name, bytes.toString("utf8"));
    caContentDigests[name] = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
  }
  if (!ca.size) throw Error("MCP trust roots unavailable");
  async function tls(input: z.infer<typeof tlsSchema>): Promise<CredentialControlTls> {
    const root = ca.get(input.caName);
    if (!root) throw Error("MCP configured trust root unavailable");
    const [cert, key] = await Promise.all([
      readFile(input.certFile, "utf8"),
      readFile(input.keyFile, "utf8"),
    ]);
    if (
      !cert ||
      !key ||
      Buffer.byteLength(cert) > 1024 * 1024 ||
      Buffer.byteLength(key) > 1024 * 1024
    )
      throw Error("MCP transport identity unavailable");
    return { url: input.url, ca: root, cert, key, serverFingerprint: input.serverFingerprint };
  }
  const workers: McpRuntimeConfiguration["workers"] = new Map();
  for (const worker of config.workers) {
    if (workers.has(worker.zoneId)) throw Error("MCP worker zone duplicated");
    workers.set(worker.zoneId, {
      enabled: worker.enabled,
      deploymentId: worker.deploymentId,
      tls: await tls(worker.tls),
    });
  }
  return {
    infrastructure: { ...config.infrastructure, caContentDigests },
    control: await tls(config.control),
    workers,
    registrations: config.registrations,
  };
}
