/** State blocks (ADR 0119 D10): `state_get`, `state_set`, `state_delete`,
 * `state_list` over the per-automation KV.
 *
 * The concurrency model lives in two other places, on purpose: the claim
 * layer serializes the runs that share an entity (state key = entity,
 * concurrency key = entity, one document per entity), and the store's CAS +
 * writer-tag covers the writers that cannot hold the claim. These blocks
 * only surface it: a CAS miss is an `ok: false` OUTPUT the graph branches
 * on, never an error, and there is no lock block.
 *
 * Dry runs: reads run live (side-effect free, and a dry walk of a sweep
 * entrypoint is worthless against empty state); writes stub with
 * `would_execute`.
 */

import { z } from "zod";

import { registerBlock } from "./registry.ts";
import type { BlockOutcome } from "./registry.ts";
import type { RunContext } from "../context.ts";
import { StateLimitError } from "../deps.ts";
import type { EngineStateStore } from "../deps.ts";

const keySchema = z.string().min(1).max(1024);

export const stateGetConfigSchema = z.object({ key: keySchema });
export const stateSetConfigSchema = z.object({
  key: keySchema,
  value: z.unknown().refine((v) => v !== undefined, "value is required"),
  expectVersion: z.number().int().min(0).optional(),
});
export const stateDeleteConfigSchema = z.object({
  key: keySchema,
  expectVersion: z.number().int().min(0).optional(),
});
export const stateListConfigSchema = z.object({
  prefix: z.string().max(1024).optional(),
  limit: z.number().int().min(1).max(500).optional(),
});

const unavailable: BlockOutcome = {
  kind: "error",
  code: "state_store_unavailable",
  message: "the automation state store is not installed",
  retryable: false,
};

function stateStore(ctx: RunContext): EngineStateStore | null {
  return ctx.deps.state ?? null;
}

/** `<runId>:<framePath>` — the write's idempotency identity (see the store
 * module comment). The frame path, not the static block id, so each loop
 * iteration is its own writer. */
function writerTag(ctx: RunContext): string {
  return `${ctx.runId}:${ctx.currentPath ?? "unknown"}`;
}

function mapStoreError(error: unknown): BlockOutcome | null {
  if (error instanceof StateLimitError) {
    return { kind: "error", code: error.code, message: error.message, retryable: false };
  }
  return null;
}

export function registerStateBlocks(): void {
  registerBlock<z.infer<typeof stateGetConfigSchema>>({
    type: "state_get",
    outputs: ["found", "value", "version"],
    configSchema: stateGetConfigSchema,
    async execute(config, ctx) {
      const store = stateStore(ctx);
      if (!store) return unavailable;
      const entry = await store.get(ctx.automationId, config.key);
      return {
        kind: "ok",
        outputs: entry
          ? { found: true, value: entry.value, version: entry.version }
          : { found: false, value: null, version: 0 },
      };
    },
  });

  registerBlock<z.infer<typeof stateSetConfigSchema>>({
    type: "state_set",
    outputs: ["ok", "version", "current_version", "current_value"],
    configSchema: stateSetConfigSchema,
    async execute(config, ctx) {
      const store = stateStore(ctx);
      if (!store) return unavailable;
      if (ctx.dryRun) {
        return {
          kind: "ok",
          outputs: {
            ok: true,
            version: 0,
            dry_run: true,
            would_execute: {
              key: config.key,
              ...(config.expectVersion !== undefined ? { expect_version: config.expectVersion } : {}),
            },
          },
        };
      }
      try {
        const result = await store.set(ctx.automationId, config.key, config.value, {
          writer: writerTag(ctx),
          ...(config.expectVersion !== undefined ? { expectVersion: config.expectVersion } : {}),
        });
        if (result.ok) return { kind: "ok", outputs: { ok: true, version: result.version } };
        return {
          kind: "ok",
          outputs: {
            ok: false,
            current_version: result.current?.version ?? 0,
            current_value: result.current?.value ?? null,
          },
        };
      } catch (error) {
        const mapped = mapStoreError(error);
        if (mapped) return mapped;
        throw error;
      }
    },
  });

  registerBlock<z.infer<typeof stateDeleteConfigSchema>>({
    type: "state_delete",
    outputs: ["ok", "deleted", "current_version"],
    configSchema: stateDeleteConfigSchema,
    async execute(config, ctx) {
      const store = stateStore(ctx);
      if (!store) return unavailable;
      if (ctx.dryRun) {
        return {
          kind: "ok",
          outputs: { ok: true, deleted: false, dry_run: true, would_execute: { key: config.key } },
        };
      }
      try {
        const result = await store.delete(ctx.automationId, config.key, {
          ...(config.expectVersion !== undefined ? { expectVersion: config.expectVersion } : {}),
        });
        if (result.ok) return { kind: "ok", outputs: { ok: true, deleted: result.deleted } };
        return {
          kind: "ok",
          outputs: { ok: false, deleted: false, current_version: result.current.version },
        };
      } catch (error) {
        const mapped = mapStoreError(error);
        if (mapped) return mapped;
        throw error;
      }
    },
  });

  registerBlock<z.infer<typeof stateListConfigSchema>>({
    type: "state_list",
    outputs: ["entries", "count", "truncated"],
    configSchema: stateListConfigSchema,
    async execute(config, ctx) {
      const store = stateStore(ctx);
      if (!store) return unavailable;
      const result = await store.list(ctx.automationId, {
        ...(config.prefix !== undefined ? { prefix: config.prefix } : {}),
        ...(config.limit !== undefined ? { limit: config.limit } : {}),
      });
      return {
        kind: "ok",
        outputs: {
          entries: result.entries.map((e) => ({ key: e.key, value: e.value, version: e.version })),
          count: result.entries.length,
          truncated: result.truncated,
        },
      };
    },
  });
}
