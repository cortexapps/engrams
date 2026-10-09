/** Profile capability cards: what each active profile IS, shaped for a
 * model that picks between profiles (the `list_profiles` block feeding a
 * `decide` choice question).
 *
 * A card carries the profile's described purpose plus the facts that tell
 * profiles apart in practice: the repositories in its workspace (the
 * strongest "which codebase" signal), its skills, the providers of the
 * connections it is granted, and its network reach. Env var NAMES only —
 * values never leave the store.
 */

import { makeIntegrationConnectionStore, type IntegrationConnectionStore } from "../db/integration-connections.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import type { ChoiceOption } from "./engine/blocks/decide.ts";

export interface ProfileCard {
  id: string;
  name: string;
  description: string;
  /** "owner/name" when the remote parses, the in-guest path otherwise. */
  repos: string[];
  skills: string[];
  /** Providers of the connections the profile is granted (e.g. "github"). */
  integrations: string[];
  /** Egress allow-list entries (hosts + patterns). */
  allowHosts: string[];
  envVarNames: string[];
}

/** Choice options for a `decide` question, one per card. The value is the
 * profile id; the label is the name, made unique (`Name (2)`) because the
 * model answers by label. The option carries only what tells profiles apart
 * by PURPOSE — the description, the repositories, the integrations. Network
 * hosts, env var names and skill bundles cost most of the tokens and, in a
 * routing benchmark against real history, bought no accuracy. Empty facts are
 * left out. Pure. */
export function profileOptions(cards: ProfileCard[]): ChoiceOption[] {
  const seen = new Map<string, number>();
  return cards.map((card) => {
    const count = (seen.get(card.name) ?? 0) + 1;
    seen.set(card.name, count);
    const description: Record<string, unknown> = {};
    if (card.description.trim()) description["purpose"] = card.description.trim();
    if (card.repos.length) description["repositories"] = card.repos;
    if (card.integrations.length) description["integrations"] = card.integrations;
    return {
      value: card.id,
      label: count === 1 ? card.name : `${card.name} (${count})`,
      ...(Object.keys(description).length ? { description } : {}),
    };
  });
}

export interface ProfileCardDeps {
  profiles?: ProfileStore;
  connections?: IntegrationConnectionStore;
}

/** Every active profile's card, ordered by name. */
export async function loadProfileCards(deps: ProfileCardDeps = {}): Promise<ProfileCard[]> {
  const profiles = deps.profiles ?? makeProfileStore();
  const connections = deps.connections ?? makeIntegrationConnectionStore();
  const [rows, conns] = await Promise.all([
    profiles.list({ includeArchived: false }),
    connections.list(),
  ]);
  const providerById = new Map(conns.map((c) => [c.id, c.provider]));
  return rows.map((r) => ({
    id: r.id,
    name: r.name,
    description: r.description,
    repos: r.repos.map((repo) =>
      repo.remote ? `${repo.remote.owner}/${repo.remote.name}` : repo.path,
    ),
    skills: r.skills,
    integrations: [
      ...new Set(
        r.integrationGrants
          .map((g) => providerById.get(g.connectionId))
          .filter((p): p is string => p != null),
      ),
    ],
    allowHosts: [...r.network.allowHosts, ...r.network.allowHostPatterns],
    envVarNames: Object.keys(r.envVars),
  }));
}
