/** Resolve declarative connector aliases for a webhook registration. */

import { loadRegistry, type CustomConnectorSource, type WebhookAliasSpec } from "../connectors/registry.ts";
import { makeAutomationStore, type WebhookRegistrationRow } from "../db/automations.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { getDb } from "../db/client.ts";
import { SYSTEM_GITHUB_REGISTRATION_ID } from "./webhook.ts";

export interface WebhookAliasResolverDeps {
  registrations?: {
    getRegistration(id: string): Promise<WebhookRegistrationRow | null>;
  };
  connectors?: CustomConnectorSource;
}

/**
 * The GitHub App is a system registration and intentionally has no PG row.
 * Resolve it directly to the built-in GitHub connector facet; dynamic
 * registrations resolve their providerHint from the stored row.
 */
export function makeWebhookAliasResolver(deps: WebhookAliasResolverDeps = {}) {
  let registrations = deps.registrations;
  let connectors = deps.connectors;
  return async (registrationId: string): Promise<WebhookAliasSpec[]> => {
    const provider = registrationId === SYSTEM_GITHUB_REGISTRATION_ID
      ? "github"
      : (await (registrations ??= makeAutomationStore()).getRegistration(registrationId))
          ?.providerHint;
    if (!provider) return [];
    connectors ??= makeConnectorStore(getDb());
    return (await loadRegistry(connectors)).get(provider)?.webhook?.aliases ?? [];
  };
}
