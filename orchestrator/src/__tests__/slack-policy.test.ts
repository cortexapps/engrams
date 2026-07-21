/**
 * Slack CommunicationPolicy (ADR 0060 P2.10) — the provider-mechanics impl.
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
  test("wraps prior thread messages in <thread context>, with the @mention as the directive at the bottom", () => {
    const out = foldReplies(
      [
        { ts: "100.0", user: "U1", text: "msg" },
        { ts: "101.0", user: "U2", text: "hello" },
        { ts: "102.0", user: "U1", text: "whats up" },
        { ts: "103.0", user: "U1", text: "<@BOT> could you please handle this" },
      ],
      null,
      "BOT",
      "103.0", // the triggering @mention's ts
    );
    expect(out.prompt).toBe(
      "<thread context>\nmsg\nhello\nwhats up\n</thread context>\n\ncould you please handle this",
    );
    expect(out.maxTs).toBe("103.0");
  });

  test("no prior context (the mention itself starts the thread) → just the directive, no wrapper", () => {
    const out = foldReplies(
      [{ ts: "100.0", user: "U1", text: "<@BOT> do the thing" }],
      null,
      "BOT",
      "100.0",
    );
    expect(out.prompt).toBe("do the thing");
    expect(out.maxTs).toBe("100.0");
  });

  test("incremental follow-up: new messages before the new mention become the context", () => {
    const out = foldReplies(
      [
        { ts: "200.0", user: "U1", text: "actually wait" },
        { ts: "201.0", user: "U1", text: "<@BOT> also do X" },
      ],
      "100.0",
      "BOT",
      "201.0",
    );
    expect(out.prompt).toBe("<thread context>\nactually wait\n</thread context>\n\nalso do X");
    expect(out.maxTs).toBe("201.0");
  });

  test("the bot's own messages advance the cursor but never feed back into the prompt", () => {
    const out = foldReplies(
      [
        { ts: "150.0", bot_id: "B1", text: "Started a session…" },
        { ts: "160.0", user: "U1", text: "<@BOT> thanks, now add tests" },
      ],
      "100.0",
      "BOT",
      "160.0",
    );
    expect(out.prompt).toBe("thanks, now add tests");
    expect(out.maxTs).toBe("160.0");
  });

  test("no new messages → empty prompt, cursor unchanged", () => {
    expect(foldReplies([], "100.0", "BOT", undefined)).toEqual({ prompt: "", maxTs: "100.0" });
  });

  test("directive not among the replies → plain join, no wrapper (graceful fallback)", () => {
    const out = foldReplies(
      [
        { ts: "100.0", user: "U1", text: "<@BOT> please fix the build" },
        { ts: "101.0", user: "U1", text: "it fails on CI" },
      ],
      null,
      "BOT",
      "999.0", // no message carries this ts
    );
    expect(out.prompt).toBe("please fix the build\n\nit fails on CI");
    expect(out.maxTs).toBe("101.0");
  });
});

describe("makeSlackPolicy()", () => {
  function fakeClient() {
    const calls: {
      posts: any[];
      updates: any[];
      reactions: any[];
      unreacts: any[];
      replies: any[];
      uploads: any[];
    } = {
      posts: [],
      updates: [],
      reactions: [],
      unreacts: [],
      replies: [],
      uploads: [],
    };
    const client: SlackPolicyClient = {
      reactions: {
        add: async (a) => void calls.reactions.push(a),
        remove: async (a) => void calls.unreacts.push(a),
      },
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
      files: {
        uploadV2: async (a) => {
          calls.uploads.push(a);
          return {};
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

  test("per-turn lifecycle on the message: 👀 onPickup, ⏳ onWorking, then onIdle clears ⏳ and adds ✅", async () => {
    const { client, calls } = fakeClient();
    const p = policy(client);
    await p.onPickup(M);
    await p.onWorking(M);
    await p.onIdle(M);
    expect(calls.reactions).toEqual([
      { channel: "C1", timestamp: "100.0", name: "eyes" },
      { channel: "C1", timestamp: "100.0", name: "hourglass_flowing_sand" },
      { channel: "C1", timestamp: "100.0", name: "white_check_mark" },
    ]);
    expect(calls.unreacts).toEqual([
      { channel: "C1", timestamp: "100.0", name: "hourglass_flowing_sand" },
    ]);
    // no thread posts — the lifecycle is reactions-only.
    expect(calls.posts).toHaveLength(0);
  });

  test("onAssistantMessage posts a NEW bubble when ref is undefined, returning its ts", async () => {
    const { client, calls } = fakeClient();
    const ref = await policy(client).onAssistantMessage(M, "first response", undefined);
    expect(ref).toBe("posted-1");
    expect(calls.posts[0].thread_ts).toBe("100.0");
    expect(JSON.stringify(calls.posts[0].blocks)).toContain("first response");
    expect(calls.updates).toHaveLength(0);
  });

  test("onAssistantMessage EDITS the bubble in place when ref is set, returning the same ref", async () => {
    const { client, calls } = fakeClient();
    const ref = await policy(client).onAssistantMessage(M, "first\n\nsecond", "posted-1");
    expect(ref).toBe("posted-1");
    expect(calls.updates).toHaveLength(1);
    expect(calls.updates[0].ts).toBe("posted-1");
    expect(JSON.stringify(calls.updates[0].blocks)).toContain("second");
    expect(calls.posts).toHaveLength(0);
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

  test("onUserQuestion posts canonical questions from a generic tool request", async () => {
    const { client, calls } = fakeClient();
    const payload = JSON.stringify({
      run_id: "r1",
      tool_call_id: "tc-generic",
      name: "ask_user_question",
      args_json: JSON.stringify({
        questions: [
          {
            question: "Ship generically?",
            header: "Ship",
            multiSelect: false,
            options: [{ label: "Yes", description: "Deploy now" }],
          },
        ],
      }),
    });
    const ref = await policy(client).onUserQuestion(M, ev("tool_call_requested", payload));
    expect(ref).toBe("posted-1");
    expect(JSON.stringify(calls.posts[0].blocks)).toContain("Ship generically?");
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

  test("onAnswered locks a generic card from tool_result_submitted", async () => {
    const { client, calls } = fakeClient();
    await policy(client).onAnswered(
      M,
      ev(
        "tool_result_submitted",
        JSON.stringify({
          tool_call_id: "tc-generic",
          result_json: JSON.stringify({ "Ship?": ["Yes"] }),
        }),
      ),
      "posted-generic",
    );
    expect(calls.updates).toHaveLength(1);
    expect(calls.updates[0].ts).toBe("posted-generic");
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

  test("onNeutralClose posts a plain 'session is complete' note — no ❌, no reaction", async () => {
    const { client, calls } = fakeClient();
    await policy(client).onNeutralClose(M, "This session is complete. Start a new session if you'd like to continue.");
    expect(calls.posts).toHaveLength(1);
    expect(calls.posts[0].text).toContain("This session is complete");
    expect(calls.posts[0].text).not.toContain("❌");
    expect(calls.reactions).toHaveLength(0);
  });

  // ── onAsset: file forwarding (ADR 0060 — share-file artifacts into Slack).
  const SESSION = { id: "s1", webUrl: "https://e.dev/sessions/s1" };
  const fileShared = (payload: Record<string, unknown>) =>
    ev("file_shared", JSON.stringify(payload));

  test("onAsset uploads a shared file as the actual bytes into the thread", async () => {
    const { client, calls } = fakeClient();
    let fetched: { sessionId: string; artifactId: string } | undefined;
    const p = makeSlackPolicy({
      client: async () => client,
      botUserId: "BOT",
      fetchArtifact: async (sessionId, artifactId) => {
        fetched = { sessionId, artifactId };
        return { bytes: new Uint8Array([1, 2, 3]), mediaType: "image/png", fileName: "shot.png" };
      },
    });
    await p.onAsset(
      M,
      fileShared({ artifact_id: "a1", caption: "a rat", size_bytes: 3, media_type: "image/png" }),
      SESSION,
    );
    expect(fetched).toEqual({ sessionId: "s1", artifactId: "a1" });
    expect(calls.uploads).toHaveLength(1);
    expect(calls.uploads[0].channel_id).toBe("C1");
    expect(calls.uploads[0].thread_ts).toBe("100.0");
    expect(calls.uploads[0].filename).toBe("shot.png");
    expect(calls.uploads[0].initial_comment).toBe("a rat");
    expect(Array.from(calls.uploads[0].file as Buffer)).toEqual([1, 2, 3]);
    expect(calls.posts).toHaveLength(0); // the file IS the message, no text line
  });

  test("onAsset synthesizes a filename from media_type when the artifact has none, and omits an empty caption", async () => {
    const { client, calls } = fakeClient();
    const p = makeSlackPolicy({
      client: async () => client,
      botUserId: "BOT",
      fetchArtifact: async () => ({ bytes: new Uint8Array([9]), mediaType: "video/mp4", fileName: "" }),
    });
    await p.onAsset(M, fileShared({ artifact_id: "a2", size_bytes: 1, media_type: "video/mp4" }), SESSION);
    expect(calls.uploads[0].filename).toBe("engram-artifact.mp4");
    expect(calls.uploads[0].initial_comment).toBeUndefined();
  });

  test("onAsset falls back to a session link when the upload throws", async () => {
    const { client, calls } = fakeClient();
    const p = makeSlackPolicy({
      client: async () => client,
      botUserId: "BOT",
      fetchArtifact: async () => {
        throw new Error("coordinator unreachable");
      },
    });
    await p.onAsset(
      M,
      fileShared({ artifact_id: "a1", caption: "a rat", size_bytes: 10, media_type: "image/png" }),
      SESSION,
    );
    expect(calls.uploads).toHaveLength(0);
    expect(calls.posts).toHaveLength(1);
    expect(calls.posts[0].text).toBe("🔗 <https://e.dev/sessions/s1|a rat>");
  });

  test("onAsset skips the fetch entirely and links when the file is over the size cap", async () => {
    const { client, calls } = fakeClient();
    let fetchCalled = false;
    const p = makeSlackPolicy({
      client: async () => client,
      botUserId: "BOT",
      fetchArtifact: async () => {
        fetchCalled = true;
        return { bytes: new Uint8Array(), mediaType: "", fileName: "" };
      },
    });
    await p.onAsset(
      M,
      fileShared({ artifact_id: "a1", caption: "huge", size_bytes: 60 * 1024 * 1024, media_type: "video/mp4" }),
      SESSION,
    );
    expect(fetchCalled).toBe(false);
    expect(calls.uploads).toHaveLength(0);
    expect(calls.posts[0].text).toBe("🔗 <https://e.dev/sessions/s1|huge>");
  });

  test("onAsset leaves a non-file integration_asset as a one-line post (no upload)", async () => {
    const { client, calls } = fakeClient();
    await policy(client).onAsset(
      M,
      ev(
        "integration_asset",
        JSON.stringify({
          provider: "forge",
          asset_kind: "pull_request",
          surface: "asset",
          data: { number: 7, title: "Fix" },
          fetchable: { kind: "external", url: "https://gh/7" },
        }),
      ),
      SESSION,
    );
    expect(calls.uploads).toHaveLength(0);
    expect(calls.posts).toHaveLength(1);
    expect(calls.posts[0].text).toBe("🔗 <https://gh/7|PR #7: Fix>");
  });

  test("onFail reacts ❌ and posts the actionable message (terminal)", async () => {
    const { client, calls } = fakeClient();
    await policy(client).onFail(M, "Couldn't start a session.");
    expect(calls.reactions).toEqual([{ channel: "C1", timestamp: "100.0", name: "x" }]);
    expect(calls.posts[0].text).toBe("❌ Couldn't start a session.");
  });

  test("onDeliveryError reacts ⚠️ and posts a NON-fatal note (no ❌, thread lives on)", async () => {
    const { client, calls } = fakeClient();
    await policy(client).onDeliveryError(M, "Mention me again to retry.");
    expect(calls.reactions).toEqual([{ channel: "C1", timestamp: "100.0", name: "warning" }]);
    expect(calls.posts[0].text).toBe("⚠️ Mention me again to retry.");
    // distinct from onFail — never the terminal ❌.
    expect(calls.reactions.some((r) => r.name === "x")).toBe(false);
  });

  test("onDeliveryError never throws even when the post fails (best-effort notice)", async () => {
    const { client, calls } = fakeClient();
    client.chat.postMessage = async () => {
      throw new Error("slack down");
    };
    // Must resolve, not reject — a Slack blip here must not wedge the workflow.
    await policy(client).onDeliveryError(M, "retry please");
    expect(calls.reactions).toEqual([{ channel: "C1", timestamp: "100.0", name: "warning" }]);
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
