/** One-shot, idempotent boot backfill for the provider-scheme retirement
 * (ADR 0119 D5).
 *
 * Two populations move onto integration triggers:
 *
 * 1. Automations bound to the retired `github-app` system registration. The
 *    installed GitHub App already delivered their events through the App's
 *    verified ingress, so they rewrite unconditionally onto the default
 *    GitHub connection.
 * 2. Custom registrations that carried a provider scheme (github_hmac_sha256,
 *    slack_v0). These are EXTERNAL webhooks the user configured by hand (a
 *    repo or workspace POSTing to the hook URL with that registration's
 *    secret), so the set of events they received is unknowable here: it is
 *    whatever the user wired up, which may be repos where the engrams App is
 *    not installed, or a strict subset of what the App's firehose carries.
 *    Re-pointing their automations onto the provider's default connection
 *    would silently change coverage in both directions (stop firing for
 *    un-installed repos; start firing on every installation). So this
 *    population is NEVER re-pointed: the registration is kept with a
 *    `disabled_reason` (the hook URL answers 410, the UI explains, and the
 *    bound automations are named in the log) and the user re-creates the
 *    trigger deliberately — as an integration trigger with an explicit scope,
 *    or as a generic webhook. A visible stop beats a quiet drift.
 *
 * A legacy `trigger.filter` (AND of payload path → JSON value) becomes a
 * `filter` block ahead of the graph. `matchesWebhookFilter` resolved paths
 * against the redacted payload; the engine's filter block resolves against
 * the run scope, where that payload is `event.raw`, so every path is
 * prefixed and the `equals` operator carries the same structural-equality
 * semantics (see the equivalence test).
 *
 * Idempotent by construction: a rewritten automation no longer binds to the
 * registration, a deleted registration is gone, and `disabled_reason` is an
 * idempotent write. Runs fire-and-forget at boot, after the default
 * connections exist.
 */

import type {
  AutomationMetaRow,
  AutomationVersionRow,
  WebhookRegistrationRow,
} from "../db/automations.ts";
import type { IntegrationConnectionRow } from "../db/integration-connections.ts";
import type { Connector } from "../connectors/registry.ts";
import type { FilterGroup } from "./engine/conditions.ts";
import {
  ENGINE_VERSION,
  validateDefinition,
  type AutomationDefinition,
  type BlockDef,
} from "./engine/definition.ts";
import { isSafePath } from "./paths.ts";

/** The retired well-known system registration id (ADR 0102 §2, retired). */
export const RETIRED_GITHUB_APP_REGISTRATION_ID = "github-app";
export const RETIRED_PROVIDER_SCHEMES: ReadonlySet<string> = new Set([
  "github_hmac_sha256",
  "slack_v0",
]);
export const RETIRED_SCHEME_DISABLED_REASON =
  "provider signature schemes retired — recreate as an integration trigger or a generic webhook";
export const LEGACY_FILTER_BLOCK_ID = "legacy_filter";

export interface RetireWebhookStore {
  listBoundToWebhookRegistration(registrationId: string): Promise<AutomationMetaRow[]>;
  getVersion(automationId: string, version: number): Promise<AutomationVersionRow | null>;
  replaceCurrentDefinition(
    automationId: string,
    definition: AutomationDefinition,
  ): Promise<AutomationMetaRow | null>;
  listRegistrations(): Promise<WebhookRegistrationRow[]>;
  setRegistrationDisabledReason(id: string, reason: string | null): Promise<boolean>;
}

export interface RetireWebhookDeps {
  store: RetireWebhookStore;
  connections: {
    ensureDefault(provider: string, displayName: string): Promise<IntegrationConnectionRow>;
  };
  log: {
    info(bindings: Record<string, unknown>, message: string): void;
    warn(bindings: Record<string, unknown>, message: string): void;
  };
}

export interface RetireWebhookResult {
  rewritten: string[];
  disabledRegistrations: string[];
}

/** Legacy AND-of-path-equality map → one filter group over `event.raw.*`. */
export function filterGroupFromLegacyWebhookFilter(
  filter: Record<string, unknown>,
): FilterGroup {
  return {
    mode: "all",
    conditions: Object.entries(filter)
      .filter(([path]) => isSafePath(path))
      .map(([path, value]) => ({ path: `event.raw.${path}`, op: "equals", value })),
  };
}

function legacyFilterBlock(filter: Record<string, unknown>): BlockDef | null {
  const group = filterGroupFromLegacyWebhookFilter(filter);
  if (group.conditions.length === 0) return null;
  return { id: LEGACY_FILTER_BLOCK_ID, type: "filter", config: { conditions: group } };
}

/** Rewrite one automation's current version onto an integration trigger.
 * Returns false when the version is not a webhook trigger (already moved). */
async function rewriteOntoIntegrationTrigger(
  deps: RetireWebhookDeps,
  meta: AutomationMetaRow,
  provider: string,
  connectionId: string,
): Promise<boolean> {
  const version = await deps.store.getVersion(meta.id, meta.currentVersion);
  if (!version || version.trigger.kind !== "webhook") return false;
  const legacy = version.trigger;
  const filterBlock = legacy.filter !== undefined ? legacyFilterBlock(legacy.filter) : null;
  const blocks = filterBlock
    ? [filterBlock, ...version.blocks.filter((b) => b.id !== LEGACY_FILTER_BLOCK_ID)]
    : version.blocks;
  const definition = validateDefinition(
    {
      engine: ENGINE_VERSION,
      trigger: {
        kind: "integration",
        provider,
        connectionId,
        eventKeys: legacy.events,
      },
      blocks,
      inputsSchema: version.inputsSchema,
      settings: version.settings,
    },
  );
  await deps.store.replaceCurrentDefinition(meta.id, definition);
  return true;
}

export async function retireProviderWebhookSchemes(
  deps: RetireWebhookDeps,
): Promise<RetireWebhookResult> {
  const result: RetireWebhookResult = {
    rewritten: [],
    disabledRegistrations: [],
  };

  // 1. The github-app system registration: unconditional rewrite.
  const systemBound = await deps.store.listBoundToWebhookRegistration(
    RETIRED_GITHUB_APP_REGISTRATION_ID,
  );
  if (systemBound.length > 0) {
    const connection = await deps.connections.ensureDefault("github", "GitHub (default)");
    for (const meta of systemBound) {
      if (await rewriteOntoIntegrationTrigger(deps, meta, "github", connection.id)) {
        result.rewritten.push(meta.id);
      }
    }
  }

  // 2. Custom registrations carrying a retired provider scheme: visible stop,
  //    never a re-point (see the header).
  for (const registration of await deps.store.listRegistrations()) {
    if (!RETIRED_PROVIDER_SCHEMES.has(registration.verification.scheme)) continue;
    if (registration.disabledReason !== null) continue;
    await deps.store.setRegistrationDisabledReason(
      registration.id,
      RETIRED_SCHEME_DISABLED_REASON,
    );
    result.disabledRegistrations.push(registration.id);
    const bound = await deps.store.listBoundToWebhookRegistration(registration.id);
    deps.log.warn(
      {
        registrationId: registration.id,
        provider: registration.providerHint,
        boundAutomationIds: bound.map((meta) => meta.id),
      },
      "custom webhook registration retired: provider signature scheme removed; hook answers 410 until its automations are re-created as integration triggers",
    );
  }

  if (result.rewritten.length > 0 || result.disabledRegistrations.length > 0) {
    deps.log.info({ ...result }, "provider webhook scheme retirement applied");
  }
  return result;
}
