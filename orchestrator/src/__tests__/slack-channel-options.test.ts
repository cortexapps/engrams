import { describe, expect, test } from "bun:test";

import {
  describeSlackChannels,
  listSlackChannelOptions,
  type SlackChannelInfo,
  type SlackChannelPages,
} from "../rpc/automations.ts";

function pagedClient(pages: Array<{ channels: Array<{ id: string; name?: string }>; next?: string }>) {
  const calls: Array<Record<string, unknown>> = [];
  const client: SlackChannelPages = {
    conversations: {
      async list(args) {
        calls.push(args);
        const page = pages[calls.length - 1] ?? { channels: [] };
        return {
          channels: page.channels,
          response_metadata: { next_cursor: page.next ?? "" },
        };
      },
    },
  };
  return { client, calls };
}

describe("listSlackChannelOptions", () => {
  test("follows next_cursor to the end: a short page is not the whole workspace", async () => {
    // Slack answers a page with fewer than `limit` channels before the end
    // (its docs say so), so the private channel on page 3 is only reached by
    // walking the cursor.
    const { client, calls } = pagedClient([
      { channels: [{ id: "C1", name: "general" }], next: "c2" },
      { channels: [{ id: "C2", name: "eng" }], next: "c3" },
      { channels: [{ id: "C3", name: "ops-private" }, { id: "C4" }] },
    ]);
    expect(await listSlackChannelOptions(client)).toEqual([
      { key: "C1", label: "#general" },
      { key: "C2", label: "#eng" },
      { key: "C3", label: "#ops-private" },
      { key: "C4", label: "C4" },
    ]);
    expect(calls.map((c) => c["cursor"])).toEqual([undefined, "c2", "c3"]);
    expect(calls.every((c) => c["types"] === "public_channel,private_channel")).toBe(true);
    // Full-size pages: the archived filter is applied after a virtual page is
    // cut, so a small page is mostly archived channels and walks the cursor
    // for nothing.
    expect(calls.every((c) => c["limit"] === 1000)).toBe(true);
  });

  test("an empty next_cursor ends the walk after one page", async () => {
    const { client, calls } = pagedClient([{ channels: [{ id: "C1", name: "general" }] }]);
    expect(await listSlackChannelOptions(client)).toEqual([{ key: "C1", label: "#general" }]);
    expect(calls).toHaveLength(1);
  });
});

describe("describeSlackChannels", () => {
  function infoClient(names: Record<string, string | Error>) {
    const calls: string[] = [];
    const client: SlackChannelInfo = {
      conversations: {
        async info({ channel }) {
          calls.push(channel);
          const name = names[channel];
          if (name instanceof Error) throw name;
          return name === undefined ? {} : { channel: { id: channel, name } };
        },
      },
    };
    return { client, calls };
  }

  test("looks each id up once, keeps request order, and labels an unseen channel with its id", async () => {
    const { client, calls } = infoClient({
      C1: "general",
      C2: new Error("channel_not_found"),
      // C3: no name (a channel the app cannot see)
    });
    const cache = new Map();
    expect(await describeSlackChannels(client, ["C2", "C1", "C3", "C1"], cache)).toEqual([
      { key: "C2", label: "C2" },
      { key: "C1", label: "#general" },
      { key: "C3", label: "C3" },
    ]);
    expect(calls.sort()).toEqual(["C1", "C2", "C3"]);
  });

  test("a name is served from the cache until it expires", async () => {
    const { client, calls } = infoClient({ C1: "general" });
    const cache = new Map();
    let now = 0;
    const clock = () => now;
    await describeSlackChannels(client, ["C1"], cache, clock);
    now = 5 * 60_000;
    expect(await describeSlackChannels(client, ["C1"], cache, clock)).toEqual([{ key: "C1", label: "#general" }]);
    expect(calls).toEqual(["C1"]);
    now = 11 * 60_000;
    await describeSlackChannels(client, ["C1"], cache, clock);
    expect(calls).toEqual(["C1", "C1"]);
  });
});
