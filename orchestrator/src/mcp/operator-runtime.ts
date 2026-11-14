import type { McpConfig } from "./config.ts";
import { validateMcpConfig } from "./config.ts";
import { canonicalJson } from "./hash.ts";
import type { PublishedOperator } from "./operator.ts";
import type { McpRuntimeConfiguration } from "./runtime-config.ts";
import type { McpOperatorPolicy } from "./session-bindings.ts";

/** A configured transport is not authority. Read publication on every selection and
 * again at the final send fence; never keep a successful permission read as a cache. */
export function makeMcpOperatorRuntime(deps: {
  configuration: McpRuntimeConfiguration;
  read(): Promise<PublishedOperator>;
  enabled(): boolean;
}) {
  function enabled(zoneId: string) {
    return deps.enabled() && deps.configuration.workers.get(zoneId)?.enabled === true;
  }
  function worker(zoneId: string) {
    const worker = deps.configuration.workers.get(zoneId);
    if (!enabled(zoneId) || !worker) throw Error("MCP worker is disabled");
    return worker;
  }
  function selectPublished(config: McpConfig, published: PublishedOperator) {
    const configured = worker(config.zone);
    const policy = published.policies.find((policy) => policy.zoneId === config.zone);
    const endpoint = new URL(config.endpoint);
    if (!policy || !policy.authorities.includes(endpoint.host))
      throw Error("MCP destination is not published");
    worker(config.zone);
    return { policy, ...configured };
  }
  async function select(raw: McpConfig) {
    const config = validateMcpConfig(raw);
    worker(config.zone);
    return selectPublished(config, await deps.read());
  }
  return {
    enabled,
    async destinations() {
      const candidates = [
        ...(deps.configuration.destinations ?? []),
        ...deps.configuration.registrations.map((registration) => registration.resource),
      ].filter((destination) => enabled(destination.zoneId));
      if (!candidates.length) return [];
      let published: PublishedOperator;
      try {
        published = await deps.read();
      } catch {
        // Missing authority removes choices without blocking credential recovery.
        return [];
      }
      const destinations = new Map<string, { zoneId: string; endpoint: string }>();
      for (const destination of candidates) {
        try {
          selectPublished(
            validateMcpConfig({
              endpoint: destination.endpoint,
              zone: destination.zoneId,
              auth: { kind: "api_key", secretRef: "" },
            }),
            published,
          );
          destinations.set(JSON.stringify([destination.zoneId, destination.endpoint]), {
            ...destination,
          });
        } catch {
          // Configured choices are hints; live policy still decides admission.
        }
      }
      return [...destinations.entries()]
        .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
        .map(([, destination]) => destination);
    },
    select,
    workerTls: (zoneId: string) => ({ ...worker(zoneId).tls }),
    deploymentId: (zoneId: string) => worker(zoneId).deploymentId,
    async assertCurrent(config: McpConfig, expected: McpOperatorPolicy) {
      if (canonicalJson((await select(config)).policy) !== canonicalJson(expected))
        throw Error("MCP operator authority changed");
    },
    async policies() {
      return (await deps.read()).policies;
    },
    async validateDestination(config: McpConfig) {
      await select(config);
    },
  };
}
