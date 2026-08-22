/** The Slack per-channel window (ADR 0119 phase 4.6).
 *
 * The Slack events route asks, per delivery, whether a channel is on the
 * thread-brain built-in. The answer is "the built-in is enabled AND the
 * channel is a key of its `channels` input" — two columns of one row, read
 * through a short in-memory cache so the route never adds a query per Slack
 * event in steady state. SetInputs / SetEnabled on the built-in invalidate
 * the cache, so an operator's flag lands on the next delivery, not 30s later.
 */

import { getDb } from "../../db/client.ts";
import { makeAutomationStore, type AutomationStore } from "../../db/automations.ts";
import { SLACK_BRAIN_BUILTIN_KEY } from "./slack-brain.ts";

export const SLACK_FLAG_CACHE_TTL_MS = 30_000;

interface Snapshot {
  enabled: boolean;
  channels: ReadonlySet<string>;
  readAtMs: number;
}

export interface SlackFlagDeps {
  store?: Pick<AutomationStore, "getByBuiltinKey">;
  nowMs?: () => number;
}

let cached: Snapshot | null = null;
let inflight: Promise<Snapshot> | null = null;

async function load(deps: SlackFlagDeps): Promise<Snapshot> {
  const store = deps.store ?? makeAutomationStore(getDb());
  const row = await store.getByBuiltinKey(SLACK_BRAIN_BUILTIN_KEY);
  const raw = row?.inputs["channels"];
  const channels =
    raw !== null && typeof raw === "object" && !Array.isArray(raw)
      ? new Set(Object.keys(raw as Record<string, unknown>))
      : new Set<string>();
  return { enabled: row?.enabled ?? false, channels, readAtMs: (deps.nowMs ?? Date.now)() };
}

/** Is this channel served by the built-in? Cached for SLACK_FLAG_CACHE_TTL_MS. */
export async function isChannelOnAutomation(
  channelId: string,
  deps: SlackFlagDeps = {},
): Promise<boolean> {
  const now = (deps.nowMs ?? Date.now)();
  if (cached === null || now - cached.readAtMs >= SLACK_FLAG_CACHE_TTL_MS) {
    // Single-flight: a burst of deliveries on a cold cache reads once.
    inflight ??= load(deps).finally(() => {
      inflight = null;
    });
    cached = await inflight;
  }
  return cached.enabled && cached.channels.has(channelId);
}

/** Drop the cache; the next delivery re-reads. Called by the automation RPCs
 * when the slack_brain built-in's inputs or enabled flag change. */
export function invalidateSlackFlagCache(): void {
  cached = null;
}
