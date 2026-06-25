/**
 * Slack CommunicationPolicy (ADR 0059 P2.10) — the provider-mechanics impl.
 *
 * The Block Kit shaping is tested in slack-blocks.test.ts; here we pin the
 * policy's own logic: `foldReplies` (the pure thread→prompt cursor fold) and
 * the method wiring, exercised through an injected fake Slack client so no
 * engine or network is needed.
 */

import { expect, test, describe } from "bun:test";
import { foldReplies, makeSlackPolicy, type SlackPolicyClient } from "../integrations/slack-policy.ts";
import type { SourceMention } from "../workflows/thread-inbox.ts";
import type { CuratedEvent } from "../control-plane/session-events.ts";

const M: SourceMention = {
  team: "T1",
  channel: "C1",
  threadRoot: "100.0",
  user: "U1",
  ts: "100.0",
  eventId: "Ev1",
};
const ev = (kind: string, payloadJson: string): CuratedEvent => ({ idx: 0n, kind, payloadJson });

describe("foldReplies()", () => {
  test("first gather (since=null): folds human messages, strips mentions, advances the cursor", () => {
    const out = foldReplies(
      [
        { ts: "100.0", user: "U1", text: "<@BOT> please fix the build" },
        { ts: "101.0", user: "U1", text: "it fails on CI" },
      ],
      null,
      "BOT",
    );
    expect(out.prompt).toBe("please fix the build\n\nit fails on CI");
    expect(out.maxTs).toBe("101.0");
  });

  test("incremental gather: only messages strictly after `since` count", () => {
    const out = foldReplies(
      [
        { ts: "100.0", user: "U1", text: "old" },
        { ts: "200.0", user: "U1", text: "new follow-up" },
      ],
      "100.0",
      "BOT",
    );
    expect(out.prompt).toBe("new follow-up");
    expect(out.maxTs).toBe("200.0");
  });

  test("the bot's own messages advance the cursor but never feed back into the prompt", () => {
    const out = foldReplies(
      [
        { ts: "150.0", bot_id: "B1", text: "Started a session…" },
        { ts: "160.0", user: "U1", text: "thanks, now add tests" },
      ],
      "100.0",
      "BOT",
    );
    expect(out.prompt).toBe("thanks, now add tests");
    expect(out.maxTs).toBe("160.0");
  });

  test("no new messages → empty prompt, cursor unchanged", () => {
    expect(foldReplies([], "100.0", "BOT")).toEqual({ prompt: "", maxTs: "100.0" });
  });
});

describe("makeSlackPolicy()", () => {
  function fakeClient() {
    const calls: { posts: any[]; updates: any[]; reactions: any[]; replies: any[] } = {
      posts: [],
      updates: [],
      reactions: [],
      replies: [],
    };
    const client: SlackPolicyClient = {
      reactions: { add: async (a) => void calls.reactions.push(a) },
      chat: {
        postMessage: async (a) => {
          calls.posts.push(a);
          return { ts: `posted-${calls.posts.length}` };
        },
        update: async (a) => void calls.updates.push(a),
      },
      conversations: {
        replies: async (a) => {
          calls.replies.push(a);
          return { messages: [{ ts: "100.0", user: "U1", text: "<@BOT> do the thing" }] };
        },
      },
    };
    return { client, calls };
  }

  const policy = (c: SlackPolicyClient) =>
    makeSlackPolicy({ client: async () => c, botUserId: "BOT" });

  test("systemPromptAppend is a non-empty constant", () => {
    expect(policy(fakeClient().client).systemPromptAppend.length).toBeGreaterThan(0);
  });

  test("onUserQuestion posts the question blocks and returns the message ts", async () => {
    const { client, calls } = fakeClient();
    const payload = JSON.stringify({
      tool_call_id: "tc",
      questions: [{ question: "Ship it?", header: "Ship", multiSelect: false, options: [{ label: "Yes" }] }],
    });
    const ref = await policy(client).onUserQuestion(M, ev("user_question", payload));
    expect(ref).toBe("posted-1");
    expect(calls.posts[0].channel).toBe("C1");
    expect(calls.posts[0].thread_ts).toBe("100.0");
    expect(JSON.stringify(calls.posts[0].blocks)).toContain("Ship it?");
  });

  test("onAnswered updates the question message in place when a ref is known", async () => {
    const { client, calls } = fakeClient();
    await policy(client).onAnswered(
      M,
      ev("question_answered", JSON.stringify({ tool_call_id: "tc", answers: { "Ship?": ["Yes"] } })),
      "posted-1",
    );
    expect(calls.updates).toHaveLength(1);
    expect(calls.updates[0].ts).toBe("posted-1");
    expect(JSON.stringify(calls.updates[0].blocks)).toContain("Yes");
  });

  test("onComplete posts the closing summary with the message + session link", async () => {
    const { client, calls } = fakeClient();
    await policy(client).onComplete(
      M,
      { id: "s1", webUrl: "https://e.dev/sessions/s1" },
      { lastMessage: "shipped", assets: [{ label: "PR #1", url: "https://gh/1" }] },
    );
    const text = JSON.stringify(calls.posts[0].blocks);
    expect(text).toContain("shipped");
    expect(text).toContain("https://e.dev/sessions/s1");
    expect(text).toContain("https://gh/1");
  });

  test("gatherThreadContext reads replies and folds them into a prompt", async () => {
    const { client, calls } = fakeClient();
    const out = await policy(client).gatherThreadContext(M, null);
    expect(calls.replies[0].channel).toBe("C1");
    expect(calls.replies[0].ts).toBe("100.0");
    expect(out.prompt).toBe("do the thing");
    expect(out.maxTs).toBe("100.0");
  });
});
