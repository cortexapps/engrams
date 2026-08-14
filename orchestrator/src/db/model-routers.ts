import { and, asc, eq, ilike, notInArray, or, sql } from "drizzle-orm";

import { getDb } from "./client.ts";
import {
  routerModel,
  routerModelPolicy,
  routerSyncState,
} from "./schema.ts";

export type RouterModelAudience = "user" | "programmatic" | "admin";

export interface RouterModelInput {
  routerId: string;
  modelId: string;
  canonicalSlug: string;
  name: string;
  author: string | null;
  description: string | null;
  contextLength: number;
  promptPrice: string | null;
  completionPrice: string | null;
  inputModalities: string[];
  outputModalities: string[];
  supportedParameters: string[];
  huggingFaceId: string | null;
  upstream: Record<string, unknown>;
}

export interface RouterModelRow extends RouterModelInput {
  available: boolean;
  enabled: boolean;
  userEnabled: boolean;
  updatedAt: Date;
}

export interface RouterSyncStateRow {
  routerId: string;
  lastSuccessfulSyncAt: Date | null;
  lastAttemptAt: Date | null;
  lastError: string | null;
}

export interface ModelRouterStore {
  listModels(routerId: string, audience: RouterModelAudience, search?: string): Promise<RouterModelRow[]>;
  getModel(routerId: string, modelId: string): Promise<RouterModelRow | null>;
  replaceCatalog(routerId: string, models: RouterModelInput[], now: Date): Promise<{ markedUnavailable: number }>;
  updatePolicy(routerId: string, modelId: string, enabled: boolean, userEnabled: boolean): Promise<RouterModelRow | null>;
  getSyncState(routerId: string): Promise<RouterSyncStateRow | null>;
  recordFailure(routerId: string, error: string, now: Date): Promise<void>;
}

export function makeModelRouterStore(db: ReturnType<typeof getDb> = getDb()): ModelRouterStore {
  const joined = {
    routerId: routerModel.routerId,
    modelId: routerModel.modelId,
    canonicalSlug: routerModel.canonicalSlug,
    name: routerModel.name,
    author: routerModel.author,
    description: routerModel.description,
    contextLength: routerModel.contextLength,
    promptPrice: routerModel.promptPrice,
    completionPrice: routerModel.completionPrice,
    inputModalities: routerModel.inputModalities,
    outputModalities: routerModel.outputModalities,
    supportedParameters: routerModel.supportedParameters,
    huggingFaceId: routerModel.huggingFaceId,
    upstream: routerModel.upstream,
    available: routerModel.available,
    enabled: sql<boolean>`coalesce(${routerModelPolicy.enabled}, false)`,
    userEnabled: sql<boolean>`coalesce(${routerModelPolicy.userEnabled}, false)`,
    updatedAt: routerModel.updatedAt,
  };

  async function queryModels(routerId: string, audience: RouterModelAudience, search?: string) {
    const policyFilter =
      audience === "user"
        ? and(eq(routerModel.available, true), eq(routerModelPolicy.enabled, true), eq(routerModelPolicy.userEnabled, true))
        : audience === "programmatic"
          ? and(eq(routerModel.available, true), eq(routerModelPolicy.enabled, true))
          : undefined;
    const needle = search?.trim();
    const searchFilter = needle
      ? or(ilike(routerModel.modelId, `%${needle}%`), ilike(routerModel.name, `%${needle}%`), ilike(routerModel.author, `%${needle}%`))
      : undefined;
    return db
      .select(joined)
      .from(routerModel)
      .leftJoin(routerModelPolicy, and(eq(routerModel.routerId, routerModelPolicy.routerId), eq(routerModel.modelId, routerModelPolicy.modelId)))
      .where(and(eq(routerModel.routerId, routerId), policyFilter, searchFilter))
      .orderBy(asc(routerModel.name), asc(routerModel.modelId));
  }

  return {
    listModels: queryModels,
    async getModel(routerId, modelId) {
      const rows = await db
        .select(joined)
        .from(routerModel)
        .leftJoin(routerModelPolicy, and(eq(routerModel.routerId, routerModelPolicy.routerId), eq(routerModel.modelId, routerModelPolicy.modelId)))
        .where(and(eq(routerModel.routerId, routerId), eq(routerModel.modelId, modelId)))
        .limit(1);
      return rows[0] ?? null;
    },
    async replaceCatalog(routerId, models, now) {
      return db.transaction(async (tx) => {
        const ids = models.map((model) => model.modelId);
        const marked = await tx
          .update(routerModel)
          .set({ available: false, updatedAt: now })
          .where(and(eq(routerModel.routerId, routerId), notInArray(routerModel.modelId, ids)))
          .returning({ id: routerModel.modelId });
        for (const model of models) {
          await tx.insert(routerModel).values({ ...model, available: true, updatedAt: now }).onConflictDoUpdate({
            target: [routerModel.routerId, routerModel.modelId],
            set: {
              canonicalSlug: model.canonicalSlug,
              name: model.name,
              author: model.author,
              description: model.description,
              contextLength: model.contextLength,
              promptPrice: model.promptPrice,
              completionPrice: model.completionPrice,
              inputModalities: model.inputModalities,
              outputModalities: model.outputModalities,
              supportedParameters: model.supportedParameters,
              huggingFaceId: model.huggingFaceId,
              upstream: model.upstream,
              available: true,
              updatedAt: now,
            },
          });
          await tx.insert(routerModelPolicy).values({ routerId, modelId: model.modelId }).onConflictDoNothing();
        }
        await tx.insert(routerSyncState).values({ routerId, lastAttemptAt: now, lastSuccessfulSyncAt: now, lastError: null }).onConflictDoUpdate({
          target: routerSyncState.routerId,
          set: { lastAttemptAt: now, lastSuccessfulSyncAt: now, lastError: null },
        });
        return { markedUnavailable: marked.length };
      });
    },
    async updatePolicy(routerId, modelId, enabled, userEnabled) {
      if (userEnabled && !enabled) throw new Error("available to users requires enabled");
      const exists = await this.getModel(routerId, modelId);
      if (!exists) return null;
      await db.insert(routerModelPolicy).values({ routerId, modelId, enabled, userEnabled, updatedAt: new Date() }).onConflictDoUpdate({
        target: [routerModelPolicy.routerId, routerModelPolicy.modelId],
        set: { enabled, userEnabled, updatedAt: new Date() },
      });
      return this.getModel(routerId, modelId);
    },
    async getSyncState(routerId) {
      const rows = await db.select().from(routerSyncState).where(eq(routerSyncState.routerId, routerId)).limit(1);
      return rows[0] ?? null;
    },
    async recordFailure(routerId, error, now) {
      await db.insert(routerSyncState).values({ routerId, lastAttemptAt: now, lastError: error }).onConflictDoUpdate({
        target: routerSyncState.routerId,
        set: { lastAttemptAt: now, lastError: error },
      });
    },
  };
}
