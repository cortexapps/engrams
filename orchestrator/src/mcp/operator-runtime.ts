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
  async function select(raw: McpConfig) {
    const config = validateMcpConfig(raw);
    const configured = worker(config.zone);
    const published = await deps.read();
    const policy = published.policies.find((policy) => policy.zoneId === config.zone);
    const endpoint = new URL(config.endpoint);
    if (!policy || !policy.authorities.includes(endpoint.host))
      throw Error("MCP destination is not published");
    worker(config.zone);
    return { policy, ...configured };
  }
  return {
    enabled,
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
