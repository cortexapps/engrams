import { createHash } from "node:crypto";
import { z } from "zod";
import { ZonePolicy } from "../../../mcp-worker/src/network.ts";
import { canonicalJson } from "./hash.ts";
import type { McpOperatorPolicy, McpOperatorPublicationIdentity } from "./session-bindings.ts";

const digest = z.string().regex(/^sha256:[0-9a-f]{64}$/);
const id = z.string().min(1).max(128);
const integer = z.number().int().min(1).max(Number.MAX_SAFE_INTEGER);
const digests = z.record(id, digest).refine((value) => Object.keys(value).length > 0);
const configuration = z
  .object({
    id,
    revision: id,
    mode: z.enum(["public", "internal"]),
    authorities: z.array(z.string()).min(1).max(4096),
    aliases: z.array(z.string()).max(4096),
    cidrs: z.array(z.string()).max(4096),
    denyCidrs: z.array(z.string()).max(4096),
    ca: z
      .string()
      .max(1024 * 1024)
      .optional(),
    limits: z
      .object({
        execute: integer.optional(),
        discover: integer.optional(),
        oauth: integer.optional(),
        schema: integer.optional(),
        toolMs: integer.optional(),
      })
      .strict()
      .optional(),
  })
  .strict();
const zone = z
  .object({
    revision: id,
    worker_digest: digest,
    authorities: z.array(z.string()).min(1).max(4096),
    aliases: z.array(z.string()).max(4096),
    configuration,
  })
  .strict();
const candidate = z
  .object({
    version: z.literal(1),
    zones: z.record(id, zone),
    deployment_identity: digest,
    network_identity: digest,
    trust_identity: digest,
    ca_content_digests: digests,
  })
  .strict();
const generation = z
  .object({
    generation: integer,
    expected_generation: integer.nullable(),
    identity: digest,
    candidate,
  })
  .strict();
const publication = z
  .object({ published: generation, pending: z.null(), admission_open: z.literal(true) })
  .strict();
export type OperatorInfrastructure = Omit<
  McpOperatorPublicationIdentity,
  "generation" | "identity"
>;
export interface PublishedOperator {
  generation: number;
  identity: string;
  policies: McpOperatorPolicy[];
}
const hash = (value: string) => `sha256:${createHash("sha256").update(value).digest("hex")}`;
function sorted<T>(value: Record<string, T>): Record<string, T> {
  return Object.fromEntries(Object.entries(value).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
}
function canonicalSet(values: string[]): string[] {
  const result = [...new Set(values)].sort();
  if (canonicalJson(result) !== canonicalJson(values))
    throw Error("operator projection is not canonical");
  return result;
}
/** Decode only the pinned coordinator publication. Configuration files specify expected
 * infrastructure identities; they cannot supply a replacement publication. */
export function parsePublishedOperator(
  bytes: Uint8Array,
  expected: OperatorInfrastructure,
): PublishedOperator {
  if (!bytes.length || bytes.length > 2 * 1024 * 1024)
    throw Error("operator publication unavailable");
  const parsed = publication.parse(
    JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)),
  );
  const p = parsed.published,
    c = p.candidate;
  if (
    p.generation !== (p.expected_generation ?? 0) + 1 ||
    !Object.keys(c.zones).length ||
    Object.keys(c.zones).length > 128
  )
    throw Error("operator generation unavailable");
  // Rust serializes struct fields in declaration order and map/object keys in sorted order.
  // The worker digest uses its own canonical encoding and is checked independently below.
  const encoded = {
    version: c.version,
    zones: sorted(
      Object.fromEntries(
        Object.entries(c.zones).map(([name, value]) => [
          name,
          {
            revision: value.revision,
            worker_digest: value.worker_digest,
            authorities: value.authorities,
            aliases: value.aliases,
            configuration: JSON.parse(canonicalJson(value.configuration)),
          },
        ]),
      ),
    ),
    deployment_identity: c.deployment_identity,
    network_identity: c.network_identity,
    trust_identity: c.trust_identity,
    ca_content_digests: sorted(c.ca_content_digests),
  };
  if (
    hash(JSON.stringify(encoded)) !== p.identity ||
    c.deployment_identity !== expected.deploymentIdentity ||
    c.network_identity !== expected.networkIdentity ||
    c.trust_identity !== expected.trustIdentity ||
    canonicalJson(c.ca_content_digests) !== canonicalJson(expected.caContentDigests)
  )
    throw Error("operator infrastructure identity changed");
  const identity: McpOperatorPublicationIdentity = {
    ...structuredClone(expected),
    generation: p.generation,
    identity: p.identity,
  };
  const hosts = new Set<string>();
  const policies = Object.entries(c.zones)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
    .map(([zoneId, value]) => {
      const config = value.configuration;
      if (
        config.id !== zoneId ||
        config.revision !== value.revision ||
        canonicalJson(canonicalSet(value.authorities)) !==
          canonicalJson([...new Set(config.authorities)].sort()) ||
        canonicalJson(canonicalSet(value.aliases)) !==
          canonicalJson([...new Set(config.aliases)].sort()) ||
        (config.ca &&
          (!config.ca.startsWith("-----BEGIN CERTIFICATE-----") ||
            config.ca.includes("PRIVATE KEY") ||
            !Object.values(c.ca_content_digests).includes(hash(config.ca))))
      )
        throw Error("operator zone projection changed");
      const worker = new ZonePolicy(config);
      if (`sha256:${worker.digest}` !== value.worker_digest)
        throw Error("operator worker configuration changed");
      for (const authority of value.authorities)
        hosts.add(new URL(`https://${authority}/`).hostname);
      for (const alias of value.aliases) hosts.add(alias);
      return {
        publication: structuredClone(identity),
        zoneId,
        policyRevision: value.revision,
        policyDigest: value.worker_digest,
        authorities: [...value.authorities],
        aliases: [...value.aliases],
      };
    });
  if (hosts.size > 4096) throw Error("operator authority set exceeds limit");
  return { generation: p.generation, identity: p.identity, policies };
}
export function makeOperatorReader(
  control: {
    readPublishedOperator(input: {
      protocol: number;
    }): Promise<{ protocol: number; publicationJson: Uint8Array }>;
  },
  expected: OperatorInfrastructure,
) {
  const trusted = structuredClone(expected);
  return {
    async read() {
      const response = await control.readPublishedOperator({ protocol: 1 });
      if (response.protocol !== 1) throw Error("operator read protocol unavailable");
      return parsePublishedOperator(response.publicationJson, trusted);
    },
  };
}
