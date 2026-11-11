/** `resolve_user` — map a chat user to an engrams user by the provider's
 * profile email, and report whether the author is linked. What to do about
 * an unlinked author is the graph's decision, so the block has no side
 * effects and runs in a dry run. `create_session` takes `user_id` as
 * `ownerUserId`, which is what makes a thread's session OWNED by the person
 * who asked: it runs with their credentials and git attribution, never with
 * the harness's programmatic org credential.
 */

import { z } from "zod";

import { resolveEngramsUser } from "../../../chat/identity.ts";
import { defaultChatProvider } from "../../../chat/registry.ts";
import { registerBlock } from "./registry.ts";

export const RESOLVE_USER_TYPE = "resolve_user";

export const resolveUserConfigSchema = z.object({
  /** The identity provider the external id belongs to. */
  provider: z.string().min(1).default(defaultChatProvider),
  /** The provider's user id. Empty = unlinked. */
  externalUserId: z.string(),
});
export type ResolveUserConfig = z.infer<typeof resolveUserConfigSchema>;

export interface ResolveUserDeps {
  resolveUser(provider: string, externalUserId: string): Promise<string | null>;
}

let runtimeDeps: ResolveUserDeps | null = null;

/** Test seam. */
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
