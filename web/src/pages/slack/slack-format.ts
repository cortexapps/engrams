/** A Slack thread workstream's key is `team:channel:thread_ts`. */
export interface ThreadKey {
  team: string;
  channel: string;
  threadTs: string;
}

export function parseThreadKey(key: string): ThreadKey | null {
  const parts = key.split(":");
  if (parts.length !== 3 || parts.some((p) => p === "")) return null;
  return { team: parts[0]!, channel: parts[1]!, threadTs: parts[2]! };
}

/** The wire carries channel IDs, not names; show the id the way Slack's own
 * URLs do. */
export function threadLabel(key: string): { channel: string; thread: string } {
  const parsed = parseThreadKey(key);
  if (!parsed) return { channel: key, thread: "" };
  return { channel: `#${parsed.channel}`, thread: parsed.threadTs };
}
