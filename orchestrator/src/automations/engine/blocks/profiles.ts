/** The list_profiles block: the org's active profiles as capability cards,
 * plus ready-made `decide` choice options (value = profile id). The pair is
 * how a graph routes work to a profile — Slack smart routing, a reviewer
 * picked from a diff — without the profile list living in the definition.
 */

import { z } from "zod";

import { profileOptions } from "../../profile-cards.ts";
import { registerBlock } from "./registry.ts";

export const listProfilesConfigSchema = z.object({
  /** Restrict the candidates to these profile ids (unknown ids are
   * skipped). Absent = every active profile. */
  ids: z.array(z.string().min(1)).optional(),
});
export type ListProfilesConfig = z.infer<typeof listProfilesConfigSchema>;

export function registerListProfilesBlock(): void {
  registerBlock<ListProfilesConfig>({
    type: "list_profiles",
    outputs: ["profiles", "options", "count"],
    configSchema: listProfilesConfigSchema,
    async execute(config, ctx) {
      const load = ctx.deps.profileCards;
      if (!load) {
        return {
          kind: "error",
          code: "profiles_runtime_unavailable",
          message: "the profile catalog is not installed",
          retryable: false,
        };
      }
      let cards = await load();
      if (config.ids) {
        const wanted = new Set(config.ids);
        cards = cards.filter((card) => wanted.has(card.id));
      }
      return {
        kind: "ok",
        outputs: { profiles: cards, options: profileOptions(cards), count: cards.length },
      };
    },
  });
}
