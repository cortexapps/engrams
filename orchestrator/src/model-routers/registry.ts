/** The model routers a session can launch through. */

import type { LanguageModel } from "ai";

import { connectorRegistry } from "../connectors/registry.ts";
import type { RouterModelInput } from "../db/model-routers.ts";
import { OPENROUTER } from "../providers/openrouter/router.ts";
import type { DecisionsClient } from "./decisions.ts";

export const ROUTER_PROTOCOLS = ["anthropic_messages", "openai_responses"] as const;
export type RouterProtocol = (typeof ROUTER_PROTOCOLS)[number];

export type CatalogModel = Omit<RouterModelInput, "routerId">;

/** What a provider declares to be a model router. */
export interface ModelRouterSpec {
  id: string;
  label: string;
  description: string;
  /** The connector that holds the router's key and host. */
  connector: string;
  protocols: Readonly<Record<RouterProtocol, { basePath: string }>>;
  /** The model a routed launch uses when neither the task nor the profile names one. */
  defaultModel: string;
  /** The model that titles a task from its first prompt. */
  titleModel: string;
  catalog: {
    path: string;
    refreshIntervalMs: number;
    parse(body: Uint8Array): CatalogModel[];
  };
  /** The public page for a model. */
  modelUrl(modelId: string): string;
  /** Chat models for calls the orchestrator makes itself. */
  chat(apiKey: string, host: string): (modelId: string) => LanguageModel;
  /** Present when the router has a native decisions endpoint. */
  decisions?(apiKey: string, host: string): DecisionsClient;
}

export interface ModelRouterDefinition extends ModelRouterSpec {
  /** The org secret that holds the key: the connector's one injected secret. */
  credentialSecret: string;
}

function define(spec: ModelRouterSpec): ModelRouterDefinition {
  const credential = connectorRegistry().get(spec.connector)?.credential;
  const injects = credential?.source === "inject" ? credential.injects : [];
  const secretRef = injects.length === 1 ? injects[0]?.secretRef : undefined;
  if (!secretRef) {
    throw new Error(`model router ${spec.id}: connector ${spec.connector} must inject exactly one secret`);
  }
  return { ...spec, credentialSecret: secretRef };
}

const REGISTRY = new Map([OPENROUTER].map((spec) => [spec.id, define(spec)]));

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

export type RouterSelection =
  | { ok: true; router: ModelRouterDefinition; protocol: RouterProtocol }
  | {
      ok: false;
      field: "modelRouter" | "model";
      code: "unknown_router" | "unsupported_router" | "unknown_model";
      message: string;
    };

/** Check that `routerId` is registered, that the harness can speak to it,
 * and that `model`, when given, is in the router's catalog. */
export async function resolveRouterSelection(input: {
  routerId: string;
  harness: string;
  harnessProtocols: readonly string[];
  /** A model to look up in the router's catalog. */
  model?: { id: string; exists(routerId: string, modelId: string): Promise<boolean> };
}): Promise<RouterSelection> {
  const router = getModelRouterDefinition(input.routerId);
  if (!router) {
    return {
      ok: false,
      field: "modelRouter",
      code: "unknown_router",
      message: `model router "${input.routerId}" is not registered`,
    };
  }
  const protocol = selectRouterProtocol(router, input.harnessProtocols);
  if (!protocol) {
    return {
      ok: false,
      field: "modelRouter",
      code: "unsupported_router",
      message: `harness "${input.harness}" does not support model router "${router.id}"`,
    };
  }
  if (input.model && !(await input.model.exists(router.id, input.model.id))) {
    return {
      ok: false,
      field: "model",
      code: "unknown_model",
      message: `model "${input.model.id}" is not in router "${router.id}"`,
    };
  }
  return { ok: true, router, protocol };
}
