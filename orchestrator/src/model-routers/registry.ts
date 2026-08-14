export const ROUTER_PROTOCOLS = ["anthropic_messages", "openai_responses"] as const;
export type RouterProtocol = (typeof ROUTER_PROTOCOLS)[number];

export interface ModelRouterDefinition {
  id: string;
  label: string;
  description: string;
  credentialSecret: string;
  protocols: Readonly<Record<RouterProtocol, { baseUrl: string }>>;
  egressHosts: readonly string[];
  defaultModel: string;
  catalog: {
    provider: string;
    path: string;
    refreshIntervalMs: number;
  };
}

const OPENROUTER: ModelRouterDefinition = {
  id: "openrouter",
  label: "OpenRouter",
  description: "Shared access to tool-capable models through OpenRouter.",
  credentialSecret: "openrouter.api_key",
  protocols: {
    anthropic_messages: { baseUrl: "https://openrouter.ai/api" },
    openai_responses: { baseUrl: "https://openrouter.ai/api/v1" },
  },
  egressHosts: ["openrouter.ai"],
  defaultModel: "deepseek/deepseek-v4-pro-0813",
  catalog: {
    provider: "openrouter",
    path: "/api/v1/models/user?output_modalities=text&supported_parameters=tools&sort=newest",
    refreshIntervalMs: 6 * 60 * 60 * 1000,
  },
};

const REGISTRY = new Map([[OPENROUTER.id, OPENROUTER]]);

export function listModelRouterDefinitions(): ModelRouterDefinition[] {
  return [...REGISTRY.values()];
}

export function getModelRouterDefinition(id: string): ModelRouterDefinition | null {
  return REGISTRY.get(id) ?? null;
}

export function selectRouterProtocol(
  router: ModelRouterDefinition,
  harnessProtocols: readonly string[],
): RouterProtocol | null {
  return ROUTER_PROTOCOLS.find(
    (protocol) => protocol in router.protocols && harnessProtocols.includes(protocol),
  ) ?? null;
}
