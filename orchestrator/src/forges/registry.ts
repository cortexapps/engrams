/** The git forges, keyed by the provider name of their connector. */

import type { ReviewVariant } from "../automations/builtins/review-variant.ts";
import { providerSpecs } from "../providers/index.ts";
import type { ForgeDeps, GitForge, GitForgeSpec } from "./forge.ts";

function specs(): Array<[string, GitForgeSpec]> {
  return providerSpecs().flatMap((spec) => (spec.forge ? [[spec.provider, spec.forge]] : []));
}
// Keyed by the spec, so a test's provider list gets its own instances.
const production = new WeakMap<GitForgeSpec, GitForge>();

/** The code-review built-in of each forge. */
export function reviewVariants(): ReviewVariant[] {
  return specs().map(([, spec]) => spec.review);
}

/** A forge's code-review built-in variant; null when the provider is not a
 * forge. */
export function reviewVariantFor(provider: string): ReviewVariant | null {
  return specs().find(([name]) => name === provider)?.[1].review ?? null;
}

/** The forge for `provider`, or null when the provider is not a forge. With
 * `deps` it is a fresh instance over those seams. */
export function forgeFor(provider: string, deps?: ForgeDeps): GitForge | null {
  const spec = specs().find(([name]) => name === provider)?.[1];
  if (!spec) return null;
  if (deps) return spec.make(deps);
  let forge = production.get(spec);
  if (!forge) production.set(spec, (forge = spec.make({})));
  return forge;
}

export function requireForge(provider: string, deps?: ForgeDeps): GitForge {
  const forge = forgeFor(provider, deps);
  if (!forge) throw new Error(`"${provider}" is not a git forge`);
  return forge;
}

/** The forge that a stored row or block config with no provider means. */
export function defaultForgeProvider(): string {
  const found = specs().find(([, spec]) => spec.legacyDefault);
  if (!found) throw new Error("no git forge is the legacy default");
  return found[0];
}
