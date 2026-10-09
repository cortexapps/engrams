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
import type { ChoiceOption, DecideQuestion } from "./engine/blocks/decide.ts";

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
 * model answers by label. Empty facts are left out so they cost no tokens.
 * Pure; the benchmark builds its requests with it too. */
export function profileOptions(cards: ProfileCard[]): ChoiceOption[] {
  const seen = new Map<string, number>();
  return cards.map((card) => {
    const count = (seen.get(card.name) ?? 0) + 1;
    seen.set(card.name, count);
    const description: Record<string, unknown> = {};
    if (card.description.trim()) description["purpose"] = card.description.trim();
    if (card.repos.length) description["repositories"] = card.repos;
    if (card.skills.length) description["skills"] = card.skills;
    if (card.integrations.length) description["integrations"] = card.integrations;
    if (card.allowHosts.length) description["network"] = card.allowHosts;
    if (card.envVarNames.length) description["env_vars"] = card.envVarNames;
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

/** The routing questions a profile router asks a `decide` block: which
 * profile fits, and whether the person asks to pick one themselves. One
 * definition, so the Slack relay and the routing benchmark ask the model the
 * same thing. */
export const PROFILE_ROUTE_INSTRUCTIONS =
  "Which profile should handle this request? A profile is an agent workspace: " +
  "the repositories it contains, its integrations and its tools. Pick the profile " +
  "whose purpose and repositories fit the request best.";

export const WANTS_CHOICE_INSTRUCTIONS =
  "Does the person explicitly ask to choose the profile or workspace themselves " +
  '(for example "which profile should I use?" or "let me pick")?';

export function profileRouteQuestions(options: ChoiceOption[]): Record<string, DecideQuestion> {
  return {
    profile: { type: "choice", instructions: PROFILE_ROUTE_INSTRUCTIONS, options },
    wants_choice: { type: "yes_no", instructions: WANTS_CHOICE_INSTRUCTIONS },
  };
}
