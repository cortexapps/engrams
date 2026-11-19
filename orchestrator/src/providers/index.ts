/** Every provider. A new provider adds its spec to `list`; the role
 * registries, the event route and the action table read it from here. */

import type { IntegrationEventSource } from "../automations/integration-ingress.ts";
import { GITHUB } from "./github/index.ts";
import { GITLAB } from "./gitlab/index.ts";
import { LINEAR } from "./linear/index.ts";
import { LITELLM } from "./litellm/index.ts";
import { OPENROUTER } from "./openrouter/index.ts";
import type { ProviderSpec } from "./provider.ts";
import { SLACK } from "./slack/index.ts";
import { VANTA } from "./vanta/index.ts";

// Read on first use: a provider module can load before this one, and its
// spec is not initialized until its own module body ends. The order is the
// order a role lists its providers in.
const list = (): ProviderSpec[] => [GITHUB, GITLAB, SLACK, LINEAR, VANTA, OPENROUTER, LITELLM];

let specs: ProviderSpec[] | null = null;
let override: ProviderSpec[] | null = null;

export function providerSpecs(): ProviderSpec[] {
  return override ?? (specs ??= list());
}

/** Test seam: the providers the product sees, until `null` restores the
 * real list. */
export function setProviderSpecsForTest(next: ProviderSpec[] | null): void {
  override = next;
}

export function eventSources(): IntegrationEventSource[] {
  return providerSpecs().flatMap((spec) => (spec.events ? [spec.events] : []));
}
