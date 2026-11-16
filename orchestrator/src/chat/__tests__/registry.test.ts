/** The chat registry reads the provider list: a second chat provider needs
 * no change outside its own spec. */

import { afterEach, describe, expect, test } from "bun:test";

import { providerSpecs, setProviderSpecsForTest } from "../../providers/index.ts";
import { SLACK_THREADS } from "../../providers/slack/threads-variant.ts";
import type { ChatProvider, ChatProviderSpec } from "../chat.ts";
import { chatFor, chatSpecs, defaultChatProvider, threadsBuiltinKey, threadsVariants } from "../registry.ts";

const FAKE_THREADS = { ...SLACK_THREADS, provider: "fakechat", key: "fakechat_threads" };

const fakeChat: ChatProvider = {
  provider: "fakechat",
  policy: () => {
    throw new Error("not used");
  },
  userEmail: async () => null,
  listChannels: async () => [],
  describeChannels: async () => [],
  plainText: (text) => text,
  threadHandle: ({ channel, threadRoot }) => `fakechat:${channel}:${threadRoot}`,
};

const FAKE_SPEC: ChatProviderSpec = { make: () => fakeChat, threads: FAKE_THREADS };

afterEach(() => setProviderSpecsForTest(null));

describe("chat registry", () => {
  test("a second chat provider is served next to Slack", () => {
    setProviderSpecsForTest([...providerSpecs(), { provider: "fakechat", chat: FAKE_SPEC }]);

    expect(chatFor("fakechat")).toBe(fakeChat);
    expect(chatSpecs().map(({ provider }) => provider)).toEqual(["slack", "fakechat"]);
    expect(chatSpecs().find(({ provider }) => provider === "fakechat")?.spec).toBe(FAKE_SPEC);
    expect(threadsVariants().map((variant) => variant.key)).toEqual(["slack_brain", "fakechat_threads"]);
    expect(threadsBuiltinKey("fakechat")).toBe("fakechat_threads");
    // Slack stays the provider a row with none means.
    expect(defaultChatProvider()).toBe("slack");
  });

  test("restoring the real list drops the fake provider", () => {
    setProviderSpecsForTest([{ provider: "fakechat", chat: FAKE_SPEC }]);
    expect(chatFor("slack")).toBeNull();
    setProviderSpecsForTest(null);
    expect(chatFor("fakechat")).toBeNull();
    expect(chatFor("slack")?.provider).toBe("slack");
  });
});
