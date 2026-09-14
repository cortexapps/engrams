/** `resolve_user` — map a provider identity to an engrams user.
 *
 * The Slack thread brain's identity gate (legacy `slack-thread.ts`
 * resolveUser) as a plain lookup: resolve by the provider's profile email
 * (ADR 0060 D4) and report whether the author is linked. What to do about an
 * unlinked author is the graph's decision — the built-in posts the legacy
 * "log in first" message and ends `filtered` — so the block itself has no
 * side effects and runs in a dry run. `create_session` takes `user_id` as
 * `ownerUserId`, which is what makes a thread's session OWNED by the person
 * who asked: it runs with their credentials and git attribution, never with
 * the harness's programmatic org credential.
 */

import { z } from "zod";

import { resolveEngramsUser } from "../../../integrations/slack-identity.ts";
import { registerBlock } from "./registry.ts";

export const RESOLVE_USER_TYPE = "resolve_user";

export const resolveUserConfigSchema = z.object({
  /** The identity provider the external id belongs to. */
  provider: z.string().min(1).default("slack"),
  /** The provider's user id (a Slack `U…` id). Empty = unlinked. */
  externalUserId: z.string(),
});
export type ResolveUserConfig = z.infer<typeof resolveUserConfigSchema>;

export interface ResolveUserDeps {
  resolveUser(provider: string, externalUserId: string): Promise<string | null>;
}

let runtimeDeps: ResolveUserDeps | null = null;

/** Test seam; production resolves by the provider profile's email. */
export function setResolveUserDeps(deps: ResolveUserDeps | null): void {
  runtimeDeps = deps;
}

function deps(): ResolveUserDeps {
  return runtimeDeps ?? { resolveUser: resolveEngramsUser };
}

export function registerResolveUserBlock(): void {
  registerBlock<ResolveUserConfig>({
    type: RESOLVE_USER_TYPE,
    outputs: ["found", "user_id"],
    configSchema: resolveUserConfigSchema,
    async execute(config) {
      const userId =
        config.externalUserId === ""
          ? null
          : await deps().resolveUser(config.provider, config.externalUserId);
      return { kind: "ok", outputs: { found: userId !== null, user_id: userId } };
    },
  });
}
