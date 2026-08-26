/** Instance-scoped state (ADR 0120 addendum): a transparent prefix
 * decorator over EngineStateStore. Installed by the interpreter when the
 * run snapshot carries an instanceId, so state blocks, CAS, and writer
 * semantics stay untouched — two workstreams of one automation can never
 * see each other's documents. Injective because instance ids contain no
 * "/". Strict in v1: an instanced automation has NO automation-global
 * state escape hatch (a deliberate, revisitable decision — see ADR 0120).
 */

import type { EngineStateStore } from "./deps.ts";

export function instanceStatePrefix(instanceId: string): string {
  return `i/${instanceId}/`;
}

export function instanceScopedState(
  inner: EngineStateStore,
  instanceId: string,
): EngineStateStore {
  const prefix = instanceStatePrefix(instanceId);
  return {
    get: (automationId, key) => inner.get(automationId, prefix + key),
    set: (automationId, key, value, opts) => inner.set(automationId, prefix + key, value, opts),
    delete: (automationId, key, opts) => inner.delete(automationId, prefix + key, opts),
    async list(automationId, opts) {
      const result = await inner.list(automationId, {
        ...(opts?.limit !== undefined ? { limit: opts.limit } : {}),
        prefix: prefix + (opts?.prefix ?? ""),
      });
      return {
        entries: result.entries.map((entry) => ({
          ...entry,
          key: entry.key.startsWith(prefix) ? entry.key.slice(prefix.length) : entry.key,
        })),
        truncated: result.truncated,
      };
    },
  };
}
