/** The registered git forges, by connector provider. */

import { GITHUB_FORGE } from "../providers/github/forge.ts";
import type { ReviewVariant } from "../automations/builtins/review-variant.ts";
import type { ForgeDeps, GitForge, GitForgeSpec } from "./forge.ts";

const SPECS: ReadonlyMap<string, GitForgeSpec> = new Map(
  [GITHUB_FORGE].map((spec) => [spec.provider, spec]),
);
const production = new Map<string, GitForge>();

export function forgeProviders(): string[] {
  return [...SPECS.keys()];
}

/** The code-review built-in of each forge that has one. */
export function reviewVariants(): ReviewVariant[] {
  return [...SPECS.values()].flatMap((spec) => (spec.review ? [spec.review] : []));
}

/** The key of a forge's code-review built-in; null when it has none. */
export function reviewBuiltinKey(provider: string): string | null {
  return SPECS.get(provider)?.review?.key ?? null;
}

/** The forge for `provider`, or null when the provider is not a forge. With
 * `deps` it is a fresh instance over those seams. */
export function forgeFor(provider: string, deps?: ForgeDeps): GitForge | null {
  const spec = SPECS.get(provider);
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
  for (const spec of SPECS.values()) if (spec.legacyDefault) return spec.provider;
  throw new Error("no git forge is the legacy default");
}
