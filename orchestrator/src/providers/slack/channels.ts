/** The Slack channels the app can see, as key options. */

import { log as rootLog } from "../../log.ts";
import type { ChannelOption } from "../../chat/chat.ts";

const log = rootLog.child({ component: "slack" });

/** The page shape `conversations.list` answers with (the `WebClient` method,
 * narrowed to what the walk reads). */
export interface SlackChannelPages {
  conversations: {
    list(args: {
      limit: number;
      exclude_archived: boolean;
      types: string;
      cursor?: string;
    }): Promise<{
      channels?: Array<{ id?: string; name?: string }>;
      response_metadata?: { next_cursor?: string };
    }>;
  };
}

/** Slack's ceiling for one `conversations.list` page. */
const SLACK_CHANNEL_PAGE_SIZE = 1000;
/** Pages of `conversations.list` to walk before giving up — 100k virtual
 * rows, archived channels included (see below). A workspace past that
 * lists its first 100k and the log says so. */
const SLACK_CHANNEL_PAGE_LIMIT = 100;

/** `conversations.info`, narrowed to what the lookup reads. */
export interface SlackChannelInfo {
  conversations: {
    info(args: { channel: string }): Promise<{ channel?: { id?: string; name?: string } }>;
  };
}

/** Channel names change rarely; a page re-render must not be a Slack call
 * per thread row. */
const SLACK_CHANNEL_NAME_TTL_MS = 10 * 60_000;
const SLACK_CHANNEL_LOOKUP_CONCURRENCY = 4;
const slackChannelNameCache = new Map<string, { label: string; expiresAt: number }>();

/** "#name" for each channel id, by `conversations.info` — a lookup of the
 * ids a page shows, never a walk of the workspace. A channel the app cannot
 * see (not a member of a private one, or gone) is labelled with its id.
 * Request order is kept; duplicates collapse. */
export async function describeSlackChannels(
  client: SlackChannelInfo,
  keys: readonly string[],
  cache: Map<string, { label: string; expiresAt: number }> = slackChannelNameCache,
  now: () => number = Date.now,
): Promise<ChannelOption[]> {
  const ids = [...new Set(keys)];
  const labels = new Map<string, string>();
  const pending: string[] = [];
  for (const id of ids) {
    const hit = cache.get(id);
    if (hit && hit.expiresAt > now()) labels.set(id, hit.label);
    else pending.push(id);
  }
  const lookup = async (id: string) => {
    let label = id;
    try {
      const result = await client.conversations.info({ channel: id });
      if (result.channel?.name) label = `#${result.channel.name}`;
    } catch (error) {
      log.warn({ channel: id, err: error }, "slack channel lookup failed; showing the id");
    }
    labels.set(id, label);
    cache.set(id, { label, expiresAt: now() + SLACK_CHANNEL_NAME_TTL_MS });
  };
  for (let i = 0; i < pending.length; i += SLACK_CHANNEL_LOOKUP_CONCURRENCY) {
    await Promise.all(pending.slice(i, i + SLACK_CHANNEL_LOOKUP_CONCURRENCY).map(lookup));
  }
  return ids.map((id) => ({ key: id, label: labels.get(id) ?? id }));
}

/** Every channel the app can see, public and private alike (a thread in a
 * private channel needs a name as much as a public one's), as key options.
 * Slack pages the list by cursor, and `exclude_archived` is applied AFTER
 * a virtual page of `limit` rows is cut — a workspace with years of
 * archived channels answers every page with a handful of live ones (one
 * in ten was seen in practice). One page is never the whole workspace,
 * so the walk follows `next_cursor` until it is empty. */
export async function listSlackChannelOptions(
  client: SlackChannelPages,
): Promise<ChannelOption[]> {
  const options: ChannelOption[] = [];
  let cursor: string | undefined;
  for (let page = 0; ; page += 1) {
    if (page === SLACK_CHANNEL_PAGE_LIMIT) {
      log.warn(
        { pages: page, channels: options.length },
        "slack channel list: page cap reached; the list is incomplete",
      );
      break;
    }
    const result = await client.conversations.list({
      limit: SLACK_CHANNEL_PAGE_SIZE,
      exclude_archived: true,
      types: "public_channel,private_channel",
      ...(cursor !== undefined ? { cursor } : {}),
    });
    for (const c of result.channels ?? []) {
      if (typeof c.id !== "string") continue;
      options.push({ key: c.id, label: c.name ? `#${c.name}` : c.id });
    }
    const next = result.response_metadata?.next_cursor;
    if (!next) break;
    cursor = next;
  }
  return options;
}
