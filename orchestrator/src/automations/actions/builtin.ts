/** The built-in action table (ADR 0119 D5).
 *
 * A connector `actions[]` entry with `execute.kind: "builtin"` names an id
 * `<provider>.<name>`. The implementation is the provider's: its spec lists
 * its actions, and a git forge gets the forge role's actions. Every provider
 * call goes through a client that a test can replace.
 */

import { forgeActions } from "../../forges/actions.ts";
import type { RunOpDeps } from "../../integrations/run-op.ts";
import { providerSpecs } from "../../providers/index.ts";

export interface BuiltinActionContext {
  runId: string;
  stepPath: string;
  /** Rendered marker for marker_comment idempotency; null otherwise. */
  marker: string | null;
}

export interface BuiltinActionDeps {
  runOpDeps?: RunOpDeps;
  /** Test seam: the client of a provider, in place of the one its actions
   * make. */
  clients?: Record<string, unknown>;
}

export type BuiltinActionFn<Client> = (
  params: Record<string, unknown>,
  ctx: BuiltinActionContext,
  client: Client,
) => Promise<Record<string, unknown>>;

/** A provider's built-in actions, by name, over the provider's client. */
export interface ProviderActions {
  names: string[];
  run(
    provider: string,
    name: string,
    params: Record<string, unknown>,
    ctx: BuiltinActionContext,
    deps: BuiltinActionDeps,
  ): Promise<Record<string, unknown>>;
}

export function defineActions<Client>(spec: {
  client(provider: string, runOpDeps?: RunOpDeps): Client | Promise<Client>;
  actions: Record<string, BuiltinActionFn<Client>>;
}): ProviderActions {
  return {
    names: Object.keys(spec.actions),
    async run(provider, name, params, ctx, deps) {
      const action = spec.actions[name];
      if (!action) throw new Error(`"${provider}" has no built-in action "${name}"`);
      // A test supplies the client of the provider it names, so the cast holds.
      const injected = deps.clients?.[provider] as Client | undefined;
      return action(params, ctx, injected ?? (await spec.client(provider, deps.runOpDeps)));
    },
  };
}

export type BuiltinAction = (
  params: Record<string, unknown>,
  ctx: BuiltinActionContext,
  deps: BuiltinActionDeps,
) => Promise<Record<string, unknown>>;

/** Every built-in action, by id. */
export function builtinActions(): Map<string, BuiltinAction> {
  const table = new Map<string, BuiltinAction>();
  for (const spec of providerSpecs()) {
    for (const actions of [spec.actions, spec.forge ? forgeActions : undefined]) {
      if (!actions) continue;
      for (const name of actions.names) {
        table.set(`${spec.provider}.${name}`, (params, ctx, deps) =>
          actions.run(spec.provider, name, params, ctx, deps),
        );
      }
    }
  }
  return table;
}
