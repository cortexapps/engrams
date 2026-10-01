import type { AutomationInstance } from "@/gen/engram/app/v1/automation_pb";

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

/** Channel id → "#name", from the channel picker's option list. */
export type ChannelNames = ReadonlyMap<string, string>;

export function channelNames(
  options: readonly { key: string; label: string }[] | undefined,
): ChannelNames {
  return new Map((options ?? []).map((o) => [o.key, o.label]));
}

/** "#name" when the id is known, else the id the way Slack's own URLs show it. */
export function channelLabel(names: ChannelNames, channelId: string): string {
  return names.get(channelId) ?? `#${channelId}`;
}

/** The wire carries channel IDs, not names; show the id the way Slack's own
 * URLs do. */
export function threadLabel(
  key: string,
  names: ChannelNames = new Map(),
): { channel: string; thread: string } {
  const parsed = parseThreadKey(key);
  if (!parsed) return { channel: key, thread: "" };
  return { channel: channelLabel(names, parsed.channel), thread: parsed.threadTs };
}

/** A thread row: the opening mention's text as the title (the workstream's
 * label, rendered at open), the place as the subtitle. */
export function describeThread(
  instance: Pick<AutomationInstance, "key" | "label">,
  names: ChannelNames,
): { title: string; subtitle: string } {
  const place = threadLabel(instance.key, names);
  const where = place.thread ? `${place.channel} · thread ${place.thread}` : place.channel;
  return { title: instance.label || where, subtitle: where };
}

/** A thread's handle (`slack:<channel>:<ts>`) as a place: "#name · thread". */
export function threadHandleLabel(handle: string, names: ChannelNames): string | undefined {
  const m = /^slack:([^:]+)(?::(.+))?$/.exec(handle);
  if (!m) return undefined;
  return `${channelLabel(names, m[1]!)}${m[2] ? " · thread" : ""}`;
}
