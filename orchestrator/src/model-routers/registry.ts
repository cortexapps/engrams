/** The model routers a session can launch through. */

import type { LanguageModel } from "ai";

import { connectorRegistry } from "../connectors/registry.ts";
import type { RouterModelInput } from "../db/model-routers.ts";
import { providerSpecs } from "../providers/index.ts";
import type { DecisionsClient } from "./decisions.ts";

export const ROUTER_PROTOCOLS = ["anthropic_messages", "openai_responses"] as const;
export type RouterProtocol = (typeof ROUTER_PROTOCOLS)[number];

export type CatalogModel = Omit<RouterModelInput, "routerId">;

/** What a router's model is used for: `default` runs a routed launch that
 * names no model, `title` titles a task, `decision` answers a decide block. */
export type RouterRole = "default" | "title" | "decision";

/** What a provider declares to be a model router. */
export interface ModelRouterSpec {
  id: string;
  label: string;
  description: string;
  /** The connector that holds the router's key and host. */
  connector: string;
  /** The protocols the router speaks. A harness needs one of them. */
  protocols: Readonly<Partial<Record<RouterProtocol, { basePath: string }>>>;
  /** Built-in model per role. A role without one needs an admin's choice. */
  models: Partial<Record<RouterRole, string>>;
  catalog: {
    path: string;
    refreshIntervalMs: number;
    parse(body: Uint8Array): CatalogModel[];
  };
  /** Whether a catalog model takes a reasoning effort. */
  takesReasoningEffort(model: Pick<CatalogModel, "supportedParameters">): boolean;
  /** The public page for a model, when the router has one. */
  modelUrl?(modelId: string): string;
  /** Chat models for calls the orchestrator makes itself. */
  chat(apiKey: string, host: string): (modelId: string) => LanguageModel;
  /** Present when the router has a native decisions endpoint. Without one,
   * a decision is a structured chat completion. */
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

const definitions = new WeakMap<ModelRouterSpec, ModelRouterDefinition>();

export function listModelRouterDefinitions(): ModelRouterDefinition[] {
  return providerSpecs().flatMap((provider) => {
    const spec = provider.router;
    if (!spec) return [];
    let definition = definitions.get(spec);
    if (!definition) definitions.set(spec, (definition = define(spec)));
    return [definition];
  });
}

export function getModelRouterDefinition(id: string): ModelRouterDefinition | null {
  return listModelRouterDefinitions().find((router) => router.id === id) ?? null;
}

/** The first protocol that the router and the harness both speak. */
function selectRouterProtocol(
  router: ModelRouterDefinition,
  harnessProtocols: readonly string[],
): { protocol: RouterProtocol; basePath: string } | null {
  for (const protocol of ROUTER_PROTOCOLS) {
    const endpoint = router.protocols[protocol];
    if (endpoint && harnessProtocols.includes(protocol)) {
      return { protocol, basePath: endpoint.basePath };
    }
  }
  return null;
}

export type RouterSelection =
  | { ok: true; router: ModelRouterDefinition; protocol: RouterProtocol; basePath: string }
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
  const endpoint = selectRouterProtocol(router, input.harnessProtocols);
  if (!endpoint) {
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
  return { ok: true, router, ...endpoint };
}
