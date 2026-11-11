import { ConnectorHostUnsetError, resolveIntegrationHost, type RunOpDeps } from "../integrations/run-op.ts";
import { Code, ConnectError, type ConnectRouter } from "@connectrpc/connect";

import { getDb } from "../db/client.ts";
import {
  makeModelRouterStore,
  type ModelRouterStore,
  type RouterModelAudience as StoreAudience,
  type RouterModelRow,
} from "../db/model-routers.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import {
  ModelRouterService,
  RouterModelAudience,
} from "../gen/engram/app/v1/model_router_pb.ts";
import { refreshRouterCatalog, type RouterCatalogDeps } from "../model-routers/catalog.ts";
import { connectedRouters, type SecretNameClient } from "../model-routers/connected.ts";
import { routerModels } from "../model-routers/models.ts";
import {
  getModelRouterDefinition,
  listModelRouterDefinitions,
  type ModelRouterDefinition,
} from "../model-routers/registry.ts";
import { requireAdmin, requireUser, type GetSession } from "./require.ts";

export interface ModelRouterServiceDeps {
  getSession?: GetSession;
  integration?: RunOpDeps;
  store?: ModelRouterStore;
  orgSecret?: SecretNameClient;
  refresh?: (routerId: string, deps: RouterCatalogDeps) => ReturnType<typeof refreshRouterCatalog>;
  now?: () => Date;
}

function modelToProto(router: ModelRouterDefinition, row: RouterModelRow) {
  return {
    routerId: row.routerId,
    id: row.modelId,
    canonicalSlug: row.canonicalSlug,
    name: row.name,
    author: row.author ?? undefined,
    description: row.description ?? undefined,
    contextLength: BigInt(row.contextLength),
    promptPrice: row.promptPrice ?? undefined,
    completionPrice: row.completionPrice ?? undefined,
    inputModalities: row.inputModalities,
    outputModalities: row.outputModalities,
    supportedParameters: row.supportedParameters,
    huggingFaceId: row.huggingFaceId ?? undefined,
    available: row.available,
    enabled: row.enabled,
    userEnabled: row.userEnabled,
    upstreamUrl: router.modelUrl?.(row.modelId) ?? "",
    updatedAt: row.updatedAt.toISOString(),
    supportsReasoning:
      row.supportedParameters.includes("reasoning") ||
      row.supportedParameters.includes("reasoning_effort"),
  };
}

export function registerModelRouters(router: ConnectRouter, deps?: ModelRouterServiceDeps): void {
  const getSession = deps?.getSession ?? getSessionFromHeaders;
  const store = deps?.store ?? makeModelRouterStore(getDb());
  const connected = async (): Promise<Set<string>> =>
    new Set((await connectedRouters(deps?.orgSecret)).map((router) => router.id));
  const refresh = deps?.refresh ?? refreshRouterCatalog;
  const now = deps?.now ?? (() => new Date());

  async function routerToProto(definition: ModelRouterDefinition, admin: boolean, configured: Set<string>) {
    const [models, state, roles, host] = await Promise.all([
      store.listModels(definition.id, "admin"),
      store.getSyncState(definition.id),
      routerModels(definition, store),
      resolveIntegrationHost(definition.connector, deps?.integration).catch((error: unknown) => {
        if (error instanceof ConnectorHostUnsetError) return null;
        throw error;
      }),
    ]);
    return {
      id: definition.id,
      label: definition.label,
      description: definition.description,
      protocols: Object.keys(definition.protocols),
      credentialSecret: admin ? definition.credentialSecret : "",
      egressHosts: host ? [host] : [],
      credentialConfigured: configured.has(definition.id),
      lastSuccessfulSyncAt: state?.lastSuccessfulSyncAt?.toISOString(),
      lastSyncError: state?.lastError ?? undefined,
      modelCount: models.length,
      availableModelCount: models.filter((model) => model.available).length,
      enabledModelCount: models.filter((model) => model.enabled).length,
      defaultModel: roles.default ?? "",
      titleModel: roles.title ?? "",
      decisionModel: roles.decision ?? "",
      connector: definition.connector,
    };
  }

  router.service(ModelRouterService, {
    async listModelRouters(_req, ctx) {
      const user = await requireUser(ctx, getSession);
      // Computed for every role — it is a boolean about a NAME, not
      // secret material. Non-admins only ever see connected routers
      // (filtered below); admins see the rest too, because the
      // settings panel is where the key gets saved in the first place.
      const configured = await connected();
      const visible = listModelRouterDefinitions().filter(
        (definition) => user.role === "admin" || configured.has(definition.id),
      );
      return {
        routers: await Promise.all(
          visible.map((definition) => routerToProto(definition, user.role === "admin", configured)),
        ),
      };
    },

    async listRouterModels(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const definition = getModelRouterDefinition(req.routerId);
      if (!definition) {
        throw new ConnectError("model router not found", Code.NotFound);
      }
      // A router without its credential is invisible to non-admins —
      // same boundary as listModelRouters, held here too so a stale
      // client cannot browse a catalog it can never launch against.
      if (user.role !== "admin" && !(await connected()).has(definition.id)) {
        throw new ConnectError("model router not found", Code.NotFound);
      }
      let audience: StoreAudience;
      switch (req.audience) {
        case RouterModelAudience.ADMIN_CATALOG:
          if (user.role !== "admin") throw new ConnectError("forbidden", Code.PermissionDenied);
          audience = "admin";
          break;
        case RouterModelAudience.PROGRAMMATIC:
          if (user.role !== "admin") throw new ConnectError("forbidden", Code.PermissionDenied);
          audience = "programmatic";
          break;
        case RouterModelAudience.UNSPECIFIED:
        case RouterModelAudience.USER:
          audience = "user";
          break;
        default:
          throw new ConnectError("invalid audience", Code.InvalidArgument);
      }
      return {
        models: (await store.listModels(req.routerId, audience, req.search))
          .map((row) => modelToProto(definition, row)),
      };
    },

    async refreshRouterModels(req, ctx) {
      await requireAdmin(ctx, getSession);
      const definition = getModelRouterDefinition(req.routerId);
      if (!definition) {
        throw new ConnectError("model router not found", Code.NotFound);
      }
      if (!(await connected()).has(definition.id)) {
        throw new ConnectError(
          `save the ${definition.label} key first — the catalog cannot be fetched without it`,
          Code.FailedPrecondition,
        );
      }
      try {
        return await refresh(req.routerId, { store });
      } catch (error) {
        throw new ConnectError(
          `router catalog refresh failed: ${error instanceof Error ? error.message : String(error)}`,
          Code.Unavailable,
        );
      }
    },

    async updateRouterModelPolicy(req, ctx) {
      await requireAdmin(ctx, getSession);
      if (req.userEnabled && !req.enabled) {
        throw new ConnectError("available to users requires enabled", Code.InvalidArgument);
      }
      const definition = getModelRouterDefinition(req.routerId);
      if (!definition) throw new ConnectError("router model not found", Code.NotFound);
      const model = await store.updatePolicy(req.routerId, req.modelId, req.enabled, req.userEnabled);
      if (!model) throw new ConnectError("router model not found", Code.NotFound);
      return { model: modelToProto(definition, model) };
    },

    async updateModelRouterSettings(req, ctx) {
      await requireAdmin(ctx, getSession);
      const definition = getModelRouterDefinition(req.routerId);
      if (!definition) throw new ConnectError("model router not found", Code.NotFound);
      // An absent field keeps the stored choice; an empty one clears it.
      const stored = await store.getSettings(definition.id);
      const chosen = (model: string | undefined, kept: string | null | undefined): string | null =>
        model === undefined ? (kept ?? null) : model.trim() || null;
      const settings = {
        defaultModel: chosen(req.defaultModel, stored?.defaultModel),
        titleModel: chosen(req.titleModel, stored?.titleModel),
        decisionModel: chosen(req.decisionModel, stored?.decisionModel),
      };
      // A native decisions endpoint serves models the chat catalog does not list.
      const inCatalog = [
        settings.defaultModel,
        settings.titleModel,
        definition.decisions ? null : settings.decisionModel,
      ];
      for (const model of inCatalog) {
        if (model && !(await store.getModel(definition.id, model))?.available) {
          throw new ConnectError(
            `model "${model}" is not in the ${definition.label} catalog`,
            Code.InvalidArgument,
          );
        }
      }
      await store.setSettings(definition.id, settings, now());
      return { router: await routerToProto(definition, true, await connected()) };
    },
  });
}
