import { Code, ConnectError, type ConnectRouter } from "@connectrpc/connect";

import { getDb } from "../db/client.ts";
import {
  makeModelRouterStore,
  type ModelRouterStore,
  type RouterModelAudience as StoreAudience,
  type RouterModelRow,
} from "../db/model-routers.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { orgSecret as defaultOrgSecret } from "../control-plane/client.ts";
import {
  ModelRouterService,
  RouterModelAudience,
} from "../gen/engram/app/v1/model_router_pb.ts";
import { refreshRouterCatalog, type RouterCatalogDeps } from "../model-routers/catalog.ts";
import { getModelRouterDefinition, listModelRouterDefinitions } from "../model-routers/registry.ts";
import { requireAdmin, requireUser, type GetSession } from "./require.ts";

interface SecretMetadataClient {
  listSecrets(req: Record<string, never>): Promise<{ secrets: Array<{ name: string }> }>;
}

export interface ModelRouterServiceDeps {
  getSession?: GetSession;
  store?: ModelRouterStore;
  orgSecret?: SecretMetadataClient;
  refresh?: (routerId: string, deps: RouterCatalogDeps) => ReturnType<typeof refreshRouterCatalog>;
}

function modelToProto(row: RouterModelRow) {
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
    upstreamUrl: `https://openrouter.ai/${row.modelId}`,
    updatedAt: row.updatedAt.toISOString(),
    supportsReasoning:
      row.supportedParameters.includes("reasoning") ||
      row.supportedParameters.includes("reasoning_effort"),
  };
}

export function registerModelRouters(router: ConnectRouter, deps?: ModelRouterServiceDeps): void {
  const getSession = deps?.getSession ?? getSessionFromHeaders;
  const store = deps?.store ?? makeModelRouterStore(getDb());
  const secrets = deps?.orgSecret ?? (defaultOrgSecret as unknown as SecretMetadataClient);
  const refresh = deps?.refresh ?? refreshRouterCatalog;

  router.service(ModelRouterService, {
    async listModelRouters(_req, ctx) {
      const user = await requireUser(ctx, getSession);
      const configured = new Set(
        user.role === "admin"
          ? (await secrets.listSecrets({})).secrets.map((secret) => secret.name)
          : [],
      );
      const routers = await Promise.all(
        listModelRouterDefinitions().map(async (definition) => {
          const [models, state] = await Promise.all([
            store.listModels(definition.id, "admin"),
            store.getSyncState(definition.id),
          ]);
          return {
            id: definition.id,
            label: definition.label,
            description: definition.description,
            protocols: Object.keys(definition.protocols),
            credentialSecret: user.role === "admin" ? definition.credentialSecret : "",
            egressHosts: [...definition.egressHosts],
            credentialConfigured: configured.has(definition.credentialSecret),
            lastSuccessfulSyncAt: state?.lastSuccessfulSyncAt?.toISOString(),
            lastSyncError: state?.lastError ?? undefined,
            modelCount: models.length,
            availableModelCount: models.filter((model) => model.available).length,
            enabledModelCount: models.filter((model) => model.enabled).length,
            defaultModel: definition.defaultModel,
          };
        }),
      );
      return { routers };
    },

    async listRouterModels(req, ctx) {
      const user = await requireUser(ctx, getSession);
      if (!getModelRouterDefinition(req.routerId)) {
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
      return { models: (await store.listModels(req.routerId, audience, req.search)).map(modelToProto) };
    },

    async refreshRouterModels(req, ctx) {
      await requireAdmin(ctx, getSession);
      if (!getModelRouterDefinition(req.routerId)) {
        throw new ConnectError("model router not found", Code.NotFound);
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
      const model = await store.updatePolicy(req.routerId, req.modelId, req.enabled, req.userEnabled);
      if (!model) throw new ConnectError("router model not found", Code.NotFound);
      return { model: modelToProto(model) };
    },
  });
}
