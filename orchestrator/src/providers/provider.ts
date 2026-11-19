/** What a provider declares. Each role is optional; `index.ts` lists every
 * provider, and that list is the only place outside a provider's directory
 * that names it. */

import type { ProviderActions } from "../automations/actions/builtin.ts";
import type { IntegrationEventSource } from "../automations/integration-ingress.ts";
import type { ChatProviderSpec } from "../chat/chat.ts";
import type { GitForgeSpec } from "../forges/forge.ts";
import type { ModelRouterSpec } from "../model-routers/registry.ts";

export interface ProviderSpec {
  /** The provider name of its connector, `connectors/<provider>.json`. */
  provider: string;
  forge?: GitForgeSpec;
  chat?: ChatProviderSpec;
  router?: ModelRouterSpec;
  /** What a delivery on the provider's event route means. The connector
   * declares the ingress. */
  events?: IntegrationEventSource;
  /** The provider's built-in automation actions. A forge's review and
   * comment actions are not listed here: the forge role supplies them. */
  actions?: ProviderActions;
}
