import type { ModelRouterStore } from "../db/model-routers.ts";
import { runIntegrationOp } from "../integrations/run-op.ts";
import { errorMessage, log } from "../log.ts";
import { getModelRouterDefinition, listModelRouterDefinitions } from "./registry.ts";
import { parseOpenRouterCatalog } from "./openrouter.ts";

export interface RouterCatalogRefreshResult {
  discovered: number;
  available: number;
  markedUnavailable: number;
}

export interface RouterCatalogDeps {
  store: ModelRouterStore;
  now?: () => Date;
  fetch?: typeof runIntegrationOp;
}

export async function refreshRouterCatalog(
  routerId: string,
  deps: RouterCatalogDeps,
): Promise<RouterCatalogRefreshResult> {
  const router = getModelRouterDefinition(routerId);
  if (!router) throw new Error(`unknown model router "${routerId}"`);
  const now = (deps.now ?? (() => new Date()))();
  try {
    const response = await (deps.fetch ?? runIntegrationOp)(router.catalog.provider, {
      method: "GET",
      path: router.catalog.path,
    });
    if (response.status < 200 || response.status >= 300) {
      throw new Error(`catalog returned HTTP ${response.status}`);
    }
    if (response.truncated) throw new Error("catalog response was truncated");
    const models = parseOpenRouterCatalog(response.body);
    if (models.length === 0) throw new Error("catalog contained no eligible models");
    const { markedUnavailable } = await deps.store.replaceCatalog(routerId, models, now);
    log.info(
      { routerId, discovered: models.length, markedUnavailable },
      "model router catalog refreshed",
    );
    return { discovered: models.length, available: models.length, markedUnavailable };
  } catch (error) {
    const message = errorMessage(error);
    await deps.store.recordFailure(routerId, message, now);
    log.warn({ routerId, err: message }, "model router catalog refresh failed; using stale cache");
    throw error;
  }
}

export class ModelRouterCatalogRefresher {
  #timer: ReturnType<typeof setInterval> | null = null;

  constructor(private readonly deps: RouterCatalogDeps) {}

  async runOnce(): Promise<void> {
    await Promise.allSettled(
      listModelRouterDefinitions().map((router) => refreshRouterCatalog(router.id, this.deps)),
    );
  }

  start(): void {
    // Catalog availability must not delay readiness. The seeded or stale cache
    // remains usable while the first refresh runs.
    void this.runOnce();
    const interval = Math.min(
      ...listModelRouterDefinitions().map((router) => router.catalog.refreshIntervalMs),
    );
    this.#timer = setInterval(() => void this.runOnce(), interval);
    this.#timer.unref?.();
  }

  stop(): void {
    if (this.#timer) clearInterval(this.#timer);
    this.#timer = null;
  }
}
