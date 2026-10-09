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
  // Every emitted label is taken, so "Dev", "Dev", "Dev (2)" gives
  // "Dev", "Dev (2)", "Dev (2) (2)" — never a repeat.
  const taken = new Set<string>();
  const unique = (name: string): string => {
    let label = name;
    for (let n = 2; taken.has(label); n += 1) label = `${name} (${n})`;
    taken.add(label);
    return label;
  };
  return cards.map((card) => {
    const description: Record<string, unknown> = {};
    if (card.description.trim()) description["purpose"] = card.description.trim();
    if (card.repos.length) description["repositories"] = card.repos;
    if (card.integrations.length) description["integrations"] = card.integrations;
    return {
      value: card.id,
      label: unique(card.name),
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

/** The routing questions as `decide` config. `options` is a ChoiceOption[]
 * or a `{ $ref }` to a list_profiles block's options (the Slack relay). */
export function profileRouteQuestions<O>(options: O) {
  return {
    profile: { type: "choice" as const, instructions: PROFILE_ROUTE_INSTRUCTIONS, options },
    wants_choice: { type: "yes_no" as const, instructions: WANTS_CHOICE_INSTRUCTIONS },
  };
}
