/**
 * LLM profile picker for Slack-triggered sessions.
 *
 * A Slack mention carries no profile choice, and there is no org default
 * profile. One `generateObject` call decides which profile serves the thread,
 * from exactly two feature sets:
 *   - capability cards: what each active profile IS (description, skills,
 *     connector providers, network reach, env var names);
 *   - histograms: which profiles this channel and this user actually used
 *     recently (task.source + task_session.profile_id).
 * The model either routes to its top profile or asks the user — `ask_user` is
 * the answer when the message explicitly requests a choice, or when the
 * candidates are genuinely ambiguous. Any model failure (no key, timeout,
 * bad output) degrades to `ask_user` too: a mention is never dropped and
 * never routed by a guess the model did not make.
 *
 * Latency is a hard budget: the histograms are two indexed reads, the model
 * call is one flash-tier request with a 5s abort. Deterministic short-circuits
 * (0 or 1 active profile) skip the model entirely.
 */

import { and, desc, eq, sql } from "drizzle-orm";
import { generateObject } from "ai";
import { z } from "zod";

import { getDb } from "../db/client.ts";
import { task as taskTable, taskSession as taskSessionTable } from "../db/schema.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import {
  makeIntegrationConnectionStore,
  type IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import { getOpenRouterClient } from "../integrations/openrouter.ts";
import { log as rootLog } from "../log.ts";

const log = rootLog.child({ component: "profile-picker" });

const PICKER_MODEL = "deepseek/deepseek-v4-flash";
/** Hard budget on the model call; past it the picker degrades to ask_user.
 *  Sized for 3 total attempts: the SDK's backoff runs them at ~0s/2s/6s. */
const PICK_TIMEOUT_MS = 10_000;
/** Bound on the message text sent to the model. */
const PROMPT_SLICE_CHARS = 2_000;
/** Recent tasks per histogram. */
const HISTOGRAM_LIMIT = 50;
/** Bound on the dropdown option list (Slack caps a select at 100). */
const MAX_OPTIONS = 25;

/** What the dropdown (and the route decision) needs to know about a profile. */
export interface ProfileOption {
  id: string;
  name: string;
  description: string;
}

/** One profile's capability card — the model-facing feature set. */
export interface ProfileCard extends ProfileOption {
  skills: string[];
  /** Providers of the connections this profile is granted (e.g. "github"). */
  connectorProviders: string[];
  /** Egress allow-list entries (hosts + patterns). */
  allowHosts: string[];
  /** Env var NAMES only — values never leave the store. */
  envVarNames: string[];
  /** Git checkouts in the image — "owner/name" when the remote parses, the
   *  in-guest path otherwise. Strong routing signal ("which codebase"). */
  repos: string[];
}

export type ProfilePick =
  | { decision: "route"; profile: ProfileOption }
  | { decision: "ask_user"; options: ProfileOption[] }
  | { decision: "none" };

export interface PickInput {
  team: string;
  channel: string;
  /** The resolved engrams user (task.created_by_user_id), not the Slack id. */
  ownerUserId: string;
  prompt: string;
}

/** profileId → recent-session count. */
export type Histogram = Map<string, number>;

export interface RawPickDecision {
  decision: "route" | "ask_user";
  ranked: string[];
  reason: string;
}

export interface ProfilePickerDeps {
  loadCandidates(): Promise<ProfileCard[]>;
  channelHistogram(team: string, channel: string): Promise<Histogram>;
  userHistogram(ownerUserId: string): Promise<Histogram>;
  /** One model call. May throw (no key, timeout, bad output) — the caller
   *  degrades to ask_user. */
  generatePick(promptText: string): Promise<RawPickDecision>;
}

export interface ProfilePicker {
  pick(input: PickInput): Promise<ProfilePick>;
}

const pickSchema = z.object({
  decision: z.enum(["route", "ask_user"]),
  ranked: z.array(z.string()).min(1),
  reason: z.string().max(500),
});

const PICKER_SYSTEM_PROMPT = `You route an incoming Slack request to one of an organization's agent profiles. A profile is a workspace configuration: what tools, integrations, and network access its sessions get.

You receive the Slack message (with thread context), a capability card for each profile (its purpose, the git repositories its workspace contains, skills, integrations, and network reach), and two usage histograms: which profiles this channel and this user used recently.

Weigh the evidence in this order:
1. Match the message against the capability cards. When the message clearly belongs to one profile's described purpose, route there — even if the histograms favor another profile. History often predates newer profiles, so a strong content match on a low-history profile beats a high count elsewhere.
2. Use the histograms only to break ties: between profiles that fit the message comparably, or for a generic message that fits any profile.

Decide:
- "route" when one profile is the right handler. Rank all profiles, best first.
- "ask_user" when the user explicitly asks to choose a profile (for example "which profile", "ask me which"), or when no card clearly fits and the histograms do not break the tie. Rank the profiles by plausibility anyway. Use "ask_user" only for genuine ambiguity — it interrupts the user.

Return JSON: {"decision": "route" | "ask_user", "ranked": [profile ids, best first], "reason": one short sentence}.
Use only profile ids from the provided cards.`;

/** Serialize the model-facing context. Pure; exported for tests. */
export function buildPickerPrompt(
  input: PickInput,
  candidates: ProfileCard[],
  channelHist: Histogram,
  userHist: Histogram,
): string {
  const name = new Map(candidates.map((c) => [c.id, c.name]));
  const histogram = (h: Histogram) =>
    [...h.entries()]
      .filter(([id]) => name.has(id))
      .sort((a, b) => b[1] - a[1])
      .map(([id, count]) => ({ profileId: id, profile: name.get(id), recentSessions: count }));
  return JSON.stringify(
    {
      message: input.prompt.slice(0, PROMPT_SLICE_CHARS),
      channelHistory: histogram(channelHist),
      userHistory: histogram(userHist),
      profiles: candidates.map((c) => ({
        id: c.id,
        name: c.name,
        description: c.description,
        repos: c.repos,
        skills: c.skills,
        integrations: c.connectorProviders,
        networkAllowHosts: c.allowHosts,
        envVarNames: c.envVarNames,
      })),
    },
    null,
    1,
  );
}

/** Candidates ordered by (channel count desc, user count desc, name) — the
 *  ranking used when the model fails or under-ranks. Pure; exported for tests. */
export function fallbackOrder(
  candidates: ProfileCard[],
  channelHist: Histogram,
  userHist: Histogram,
): ProfileCard[] {
  return [...candidates].sort(
    (a, b) =>
      (channelHist.get(b.id) ?? 0) - (channelHist.get(a.id) ?? 0) ||
      (userHist.get(b.id) ?? 0) - (userHist.get(a.id) ?? 0) ||
      a.name.localeCompare(b.name),
  );
}

/**
 * Fold the model's raw decision into a ProfilePick. Pure; exported for tests.
 * Unknown ids are dropped; the fallback order fills the tail so the dropdown
 * always lists every candidate. A "route" whose top id is invalid degrades to
 * ask_user (the model's choice, not a silent re-guess).
 */
export function normalizeDecision(
  raw: RawPickDecision,
  candidates: ProfileCard[],
  fallback: ProfileCard[],
): ProfilePick {
  const byId = new Map(candidates.map((c) => [c.id, c]));
  const ranked: ProfileCard[] = [];
  for (const id of raw.ranked) {
    const card = byId.get(id);
    if (card && !ranked.includes(card)) ranked.push(card);
  }
  for (const card of fallback) if (!ranked.includes(card)) ranked.push(card);

  const top = raw.ranked.length > 0 ? byId.get(raw.ranked[0]) : undefined;
  if (raw.decision === "route" && top) {
    return { decision: "route", profile: option(top) };
  }
  return { decision: "ask_user", options: ranked.slice(0, MAX_OPTIONS).map(option) };
}

const option = (c: ProfileCard): ProfileOption => ({
  id: c.id,
  name: c.name,
  description: c.description,
});

/** The decision flow around the injected deps. */
export function makePicker(deps: ProfilePickerDeps): ProfilePicker {
  return {
    async pick(input) {
      const candidates = await deps.loadCandidates();
      if (candidates.length === 0) return { decision: "none" };
      if (candidates.length === 1) return { decision: "route", profile: option(candidates[0]) };

      const [channelHist, userHist] = await Promise.all([
        deps.channelHistogram(input.team, input.channel),
        deps.userHistogram(input.ownerUserId),
      ]);
      const fallback = fallbackOrder(candidates, channelHist, userHist);
      try {
        const raw = await deps.generatePick(
          buildPickerPrompt(input, candidates, channelHist, userHist),
        );
        const pick = normalizeDecision(raw, candidates, fallback);
        log.info(
          { channel: input.channel, decision: pick.decision, reason: raw.reason },
          "profile picker decided",
        );
        return pick;
      } catch (err) {
        // No key / timeout / bad output — ask instead of guessing.
        log.warn({ channel: input.channel, err }, "profile picker model failed; asking the user");
        return {
          decision: "ask_user",
          options: fallback.slice(0, MAX_OPTIONS).map(option),
        };
      }
    },
  };
}

/** Count recent primary sessions per profile from a page of task rows. */
function fold(rows: { profileId: string | null }[]): Histogram {
  const h: Histogram = new Map();
  for (const r of rows) {
    if (r.profileId) h.set(r.profileId, (h.get(r.profileId) ?? 0) + 1);
  }
  return h;
}

export interface ProductionPickerDeps {
  profiles?: ProfileStore;
  connections?: IntegrationConnectionStore;
  db?: ReturnType<typeof getDb>;
}

/** Production picker: Drizzle histograms + the OpenRouter flash model. */
export function makeProfilePicker(deps: ProductionPickerDeps = {}): ProfilePicker {
  const db = deps.db ?? getDb();
  const profiles = deps.profiles ?? makeProfileStore(db);
  const connections = deps.connections ?? makeIntegrationConnectionStore(db);

  return makePicker({
    async loadCandidates() {
      const [rows, conns] = await Promise.all([
        profiles.list({ includeArchived: false }),
        connections.list(),
      ]);
      const providerById = new Map(conns.map((c) => [c.id, c.provider]));
      return rows
        // System-designated profiles (e.g. the PR reviewer) serve their own
        // workflows; a Slack mention never routes to one.
        .filter((r) => r.designation == null)
        .map((r) => ({
          id: r.id,
          name: r.name,
          description: r.description,
          repos: r.repos.map((repo) =>
            repo.remote ? `${repo.remote.owner}/${repo.remote.name}` : repo.path,
          ),
          skills: r.skills,
          connectorProviders: [
            ...new Set(
              r.integrationGrants
                .map((g) => providerById.get(g.connectionId))
                .filter((p): p is string => p != null),
            ),
          ],
          allowHosts: [...r.network.allowHosts, ...r.network.allowHostPatterns],
          envVarNames: Object.keys(r.envVars),
        }));
    },

    async channelHistogram(team, channel) {
      const rows = await db
        .select({ profileId: taskSessionTable.profileId })
        .from(taskTable)
        .innerJoin(
          taskSessionTable,
          and(eq(taskSessionTable.taskId, taskTable.id), eq(taskSessionTable.role, "primary")),
        )
        .where(
          and(
            sql`${taskTable.source}->>'provider' = 'slack'`,
            sql`${taskTable.source}->>'team' = ${team}`,
            sql`${taskTable.source}->>'channel' = ${channel}`,
          ),
        )
        .orderBy(desc(taskTable.createdAt))
        .limit(HISTOGRAM_LIMIT);
      return fold(rows);
    },

    async userHistogram(ownerUserId) {
      const rows = await db
        .select({ profileId: taskSessionTable.profileId })
        .from(taskTable)
        .innerJoin(
          taskSessionTable,
          and(eq(taskSessionTable.taskId, taskTable.id), eq(taskSessionTable.role, "primary")),
        )
        .where(eq(taskTable.createdByUserId, ownerUserId))
        .orderBy(desc(taskTable.createdAt))
        .limit(HISTOGRAM_LIMIT);
      return fold(rows);
    },

    async generatePick(promptText) {
      const openrouter = await getOpenRouterClient();
      const { object } = await generateObject({
        model: openrouter.chat(PICKER_MODEL),
        schema: pickSchema,
        system: PICKER_SYSTEM_PROMPT,
        prompt: promptText,
        abortSignal: AbortSignal.timeout(PICK_TIMEOUT_MS),
        // 3 total attempts absorb upstream 429 bursts; the abort signal
        // bounds the total and the ask_user fallback covers the rest.
        maxRetries: 2,
      });
      return object;
    },
  });
}
