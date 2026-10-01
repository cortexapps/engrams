import { describe, expect, test } from "bun:test";

import { listSlackChannelOptions, type SlackChannelPages } from "../rpc/automations.ts";

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
      { channels: [{ id: "C3", name: "nikhil-dont-spam-people" }, { id: "C4" }] },
    ]);
    expect(await listSlackChannelOptions(client)).toEqual([
      { key: "C1", label: "#general" },
      { key: "C2", label: "#eng" },
      { key: "C3", label: "#nikhil-dont-spam-people" },
      { key: "C4", label: "C4" },
    ]);
    expect(calls.map((c) => c["cursor"])).toEqual([undefined, "c2", "c3"]);
    expect(calls.every((c) => c["types"] === "public_channel,private_channel")).toBe(true);
  });

  test("an empty next_cursor ends the walk after one page", async () => {
    const { client, calls } = pagedClient([{ channels: [{ id: "C1", name: "general" }] }]);
    expect(await listSlackChannelOptions(client)).toEqual([{ key: "C1", label: "#general" }]);
    expect(calls).toHaveLength(1);
  });
});
