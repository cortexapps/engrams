/** The integration_action block (ADR 0119 D5). Phase 2 supplies the catalog
 * executor through the deps seam; until then execution returns a typed
 * unavailable error while save-time validation accepts the config shape.
 */

import { z } from "zod";

import { registerBlock } from "./registry.ts";

export const integrationActionConfigSchema = z.object({
  provider: z.string().min(1),
  actionId: z.string().regex(/^[a-z][a-z0-9_]*$/),
  connectionId: z.string().min(1).optional(),
  /** Action parameters; string values may carry Liquid templates. */
  params: z.record(z.string(), z.unknown()),
});
export type IntegrationActionConfig = z.infer<typeof integrationActionConfigSchema>;

async function renderParams(
  params: Record<string, unknown>,
  render: (template: string) => Promise<string>,
): Promise<Record<string, unknown>> {
  const rendered: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(params)) {
    if (typeof value === "string" && value.includes("${{")) {
      rendered[key] = await render(value);
    } else if (typeof value === "object" && value !== null && !Array.isArray(value)) {
      rendered[key] = await renderParams(value as Record<string, unknown>, render);
    } else {
      rendered[key] = value;
    }
  }
  return rendered;
}

export function registerIntegrationActionBlock(): void {
  registerBlock<IntegrationActionConfig>({
    type: "integration_action",
    configSchema: integrationActionConfigSchema,
    async execute(config, ctx) {
      const runtime = ctx.deps.integrationActions;
      if (!runtime) {
        return {
          kind: "error",
          code: "integration_runtime_unavailable",
          message: "the integration action runtime is not installed",
          retryable: false,
        };
      }
      if (ctx.currentBlockId === undefined) {
        return { kind: "error", code: "engine_bug", message: "currentBlockId missing", retryable: false };
      }
      const params = await renderParams(config.params, (t) => ctx.render(t));
      if (ctx.dryRun) {
        // Editor DryRun: the provider is never called. The step output
        // records what WOULD have been posted, so the run page shows it.
        return {
          kind: "ok",
          outputs: {
            dry_run: true,
            would_execute: {
              provider: config.provider,
              action_id: config.actionId,
              ...(config.connectionId !== undefined ? { connection_id: config.connectionId } : {}),
              params,
            },
          },
        };
      }
      let outputs: Record<string, unknown>;
      try {
        outputs = await runtime.execute({
          provider: config.provider,
          actionId: config.actionId,
          ...(config.connectionId !== undefined ? { connectionId: config.connectionId } : {}),
          params,
          runId: ctx.runId,
          blockId: ctx.currentBlockId,
        });
      } catch (error) {
        // The runtime raises typed errors carrying `permanent` (see
        // automations/actions/errors.ts); surface transience honestly so the
        // block's retry policy can act on 5xx/429-class failures.
        return {
          kind: "error",
          code: "integration_action_failed",
          message: error instanceof Error ? error.message : String(error),
          retryable: isTransientActionError(error),
        };
      }
      return { kind: "ok", outputs };
    },
  });
}

function isTransientActionError(error: unknown): boolean {
  return (
    error instanceof Error &&
    "permanent" in error &&
    (error as Error & { permanent: unknown }).permanent === false
  );
}
