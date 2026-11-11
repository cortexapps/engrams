/** The git forges, keyed by the provider name of their connector. */

import { GITHUB_FORGE } from "../providers/github/forge.ts";
import { GITLAB_FORGE } from "../providers/gitlab/forge.ts";
import type { ReviewVariant } from "../automations/builtins/review-variant.ts";
import type { ForgeDeps, GitForge, GitForgeSpec } from "./forge.ts";

// Built on first use: a provider module can load before this one, and its
// spec is not initialized until its own module body ends.
let specs: ReadonlyMap<string, GitForgeSpec> | null = null;
function specTable(): ReadonlyMap<string, GitForgeSpec> {
  return (specs ??= new Map([GITHUB_FORGE, GITLAB_FORGE].map((spec) => [spec.provider, spec])));
}
const production = new Map<string, GitForge>();

export function forgeProviders(): string[] {
  return [...specTable().keys()];
}

/** The code-review built-in of each forge that has one. */
export function reviewVariants(): ReviewVariant[] {
  return [...specTable().values()].flatMap((spec) => (spec.review ? [spec.review] : []));
}

/** A forge's code-review built-in variant; null when it has none. */
export function reviewVariantFor(provider: string): ReviewVariant | null {
  return specTable().get(provider)?.review ?? null;
}

/** The forge for `provider`, or null when the provider is not a forge. With
 * `deps` it is a fresh instance over those seams. */
export function forgeFor(provider: string, deps?: ForgeDeps): GitForge | null {
  const spec = specTable().get(provider);
  if (!spec) return null;
  if (deps) return spec.make(deps);
  let forge = production.get(provider);
  if (!forge) production.set(provider, (forge = spec.make({})));
  return forge;
}

export function requireForge(provider: string, deps?: ForgeDeps): GitForge {
  const forge = forgeFor(provider, deps);
  if (!forge) throw new Error(`"${provider}" is not a git forge`);
  return forge;
}

/** The forge that a stored row or block config with no provider means. */
export function defaultForgeProvider(): string {
  for (const spec of specTable().values()) if (spec.legacyDefault) return spec.provider;
  throw new Error("no git forge is the legacy default");
}
