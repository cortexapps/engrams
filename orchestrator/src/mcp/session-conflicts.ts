import type { IntegrationPolicyJson } from "../connectors/registry.ts";
import { z } from "zod";

function hostname(value: string) {
  const parsed = new URL(`https://${value}/`);
  if (parsed.username || parsed.password || parsed.pathname !== "/" || parsed.search || parsed.hash)
    throw Error("MCP direct authority invalid");
  return parsed.hostname.toLowerCase().replace(/\.$/, "");
}
function matches(pattern: string, host: string) {
  const parts = pattern.split("*").map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  return new RegExp(`^${parts.join(".*")}$`, "i").test(host);
}
/** Called after provider, native-harness and model-router policy contributions. */
export function assertMcpDirectConflicts(
  policy: IntegrationPolicyJson,
  managedHosts: readonly string[],
  reservedReferences: ReadonlySet<string>,
) {
  const hosts = new Set(managedHosts);
  const direct = [
    ...policy.network.allow_hosts,
    ...policy.injects.flatMap((i) => i.hosts),
    ...policy.secrets.flatMap((s) => s.allow_hosts),
  ];
  const patterns = [
    ...policy.network.allow_host_patterns,
    ...policy.secrets.flatMap((s) => s.allow_host_patterns),
  ];
  if (
    direct.some((value) => hosts.has(hostname(value))) ||
    patterns.some((pattern) => managedHosts.some((host) => matches(pattern, host)))
  )
    throw Error("direct integration authority conflicts with managed MCP");
  if (
    policy.secrets.some((secret) => reservedReferences.has(secret.secret_ref)) ||
    policy.injects.some((inject) => reservedReferences.has(inject.secret_ref))
  )
    throw Error("managed MCP credential cannot be injected into a session");
}
/** Validate the final frozen child policy after all launch-policy substitutions. */
export function assertMcpDirectPolicyJson(
  raw: string,
  hosts: readonly string[],
  reservedReferences: ReadonlySet<string> = new Set(),
) {
  const strings = z.array(z.string());
  const policy = z
    .object({
      network: z.object({ allow_hosts: strings, allow_host_patterns: strings }),
      injects: z.array(z.object({ hosts: strings, secret_ref: z.string() })),
      secrets: z.array(
        z.object({ secret_ref: z.string(), allow_hosts: strings, allow_host_patterns: strings }),
      ),
    })
    .parse(JSON.parse(raw));
  const direct = [
    ...policy.network.allow_hosts,
    ...policy.injects.flatMap((i) => i.hosts),
    ...policy.secrets.flatMap((s) => s.allow_hosts),
  ];
  const patterns = [
    ...policy.network.allow_host_patterns,
    ...policy.secrets.flatMap((s) => s.allow_host_patterns),
  ];
  if (
    direct.some((value) => hosts.includes(hostname(value))) ||
    patterns.some((pattern) => hosts.some((host) => matches(pattern, host))) ||
    policy.injects.some((i) => reservedReferences.has(i.secret_ref)) ||
    policy.secrets.some((s) => reservedReferences.has(s.secret_ref))
  )
    throw Error("direct integration authority conflicts with managed MCP");
}
