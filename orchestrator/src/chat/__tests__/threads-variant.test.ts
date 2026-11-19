/** The chat-threads graph with a provider that is not Slack: the provider
 * says where a mention is, and the graph admits it and chooses the profile. */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";

import { chatThreadsDefinition } from "../../automations/builtins/chat-threads.ts";
import type { ThreadsVariant } from "../../automations/builtins/threads-variant.ts";
import { makeCodeBlockRuntime } from "../../automations/code/runtime.ts";
import { evaluateAdmissionPrelude } from "../../automations/engine/admission.ts";
import { registerEngineBlocks } from "../../automations/engine/blocks/index.ts";
import { validateDefinition } from "../../automations/engine/definition.ts";
import { admissionPrelude, previewDefinition } from "../../automations/engine/preview.ts";
import { providerSpecs, setProviderSpecsForTest } from "../../providers/index.ts";
import { SLACK_THREADS } from "../../providers/slack/threads-variant.ts";
import type { ChatProvider } from "../chat.ts";

registerEngineBlocks();

/** A payload shape of its own: a room, a topic and a message id. */
const FAKE_PLACE_SOURCE = `
export default ({ event, trigger }) => {
  const note = event.raw.note ?? {};
  if (trigger.event !== "note.mention" || note.from_app) return null;
  return {
    team: String(event.raw.org ?? ""),
    channel: String(note.room ?? ""),
    thread_ts: String(note.topic ?? note.id ?? ""),
    mention_ts: String(note.id ?? ""),
    user_id: String(note.author ?? ""),
    event_id: String(event.raw.id ?? ""),
    text: String(note.text ?? ""),
    title: String(note.text ?? "").slice(0, 80),
  };
};
`.trim();

const FAKE_THREADS: ThreadsVariant = {
  ...SLACK_THREADS,
  provider: "fakechat",
  key: "fakechat_threads",
  name: "Fake chat threads",
  mentionEvents: ["note.mention"],
  place: { type: "code", config: { source: FAKE_PLACE_SOURCE, mode: "value" } },
  threadKeyTemplate: "${{ event.raw.org }}:${{ event.raw.note.room }}:${{ event.raw.note.topic }}",
  threadLabelTemplate: "${{ event.raw.note.text | truncate: 80 }}",
};

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

const MENTION = {
  id: "ev-1",
  org: "org-1",
  note: { id: "n-2", topic: "n-1", room: "ops", author: "u-1", text: "restart the worker" },
};

beforeEach(() =>
  setProviderSpecsForTest([
    ...providerSpecs(),
    { provider: "fakechat", chat: { make: () => fakeChat, threads: FAKE_THREADS } },
  ]),
);
afterEach(() => setProviderSpecsForTest(null));

describe("the chat-threads graph of a second chat provider", () => {
  const run = (inputs: Record<string, unknown>, payload: Record<string, unknown> = MENTION) => ({
    definition: chatThreadsDefinition(FAKE_THREADS),
    inputs,
    automationId: "a1",
    automationName: "threads",
    trigger: {
      kind: "integration" as const,
      receivedAt: "2026-08-21T00:00:00Z",
      eventKey: "note.mention",
      payload,
    },
    aliases: [],
    entrypointId: "main",
    code: makeCodeBlockRuntime(),
  });

  test("the definition is valid and names no Slack code in its admission", () => {
    const definition = validateDefinition(chatThreadsDefinition(FAKE_THREADS));
    const prelude = admissionPrelude(definition.blocks);
    expect(prelude.map((block) => block.id)).toEqual(["place", "facts", "admit"]);
    expect(JSON.stringify(prelude)).not.toContain("app_mention");
  });

  test("a mention is admitted with the channel's profile, else the default", async () => {
    const override = await previewDefinition(run({ channels: { ops: "p-ops" }, default_profile: "p-d" }));
    expect(override.blocks.find((block) => block.blockId === "facts")?.rendered).toMatchObject({
      value: { admit: true, channel: "ops", thread_ts: "n-1", profile_id: "p-ops", override: true, smart: false },
    });
    expect(await evaluateAdmissionPrelude(run({ channels: {}, default_profile: "p-d" }))).toEqual({ kind: "admit" });
    expect(await evaluateAdmissionPrelude(run({ channels: {}, routing: "smart" }))).toEqual({ kind: "admit" });
  });

  test("with no profile, and for the app's own note, the event is rejected", async () => {
    expect(await evaluateAdmissionPrelude(run({ channels: {} }))).toMatchObject({ kind: "reject", blockId: "admit" });
    const own = { ...MENTION, note: { ...MENTION.note, from_app: true } };
    expect(await evaluateAdmissionPrelude(run({ channels: {}, default_profile: "p-d" }, own))).toMatchObject({
      kind: "reject",
    });
  });
});
