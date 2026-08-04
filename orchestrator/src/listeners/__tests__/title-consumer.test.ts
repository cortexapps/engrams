import { describe, expect, test } from "bun:test";

import type { CuratedEvent } from "../../control-plane/session-events.ts";
import {
  makeTitleConsumer,
  parseUserPrompt,
  type TitleableTask,
} from "../title-consumer.ts";

function agentMessage(role: string, text: string): CuratedEvent {
  return { idx: 3n, kind: "agent_message", payloadJson: JSON.stringify({ role, text }) };
}

interface Harness {
  consumer: ReturnType<typeof makeTitleConsumer>;
  generated: string[];
  saved: Array<{ taskId: string; title: string }>;
}

function harness(
  task: TitleableTask | null,
  opts: {
    generate?: (prompt: string) => Promise<string>;
    isPermanent?: (err: unknown) => boolean;
  } = {},
): Harness {
  const generated: string[] = [];
  const saved: Array<{ taskId: string; title: string }> = [];
  const consumer = makeTitleConsumer({
    findTitleableTask: async () => task,
    generateTitle: async (prompt) => {
      generated.push(prompt);
      return opts.generate ? opts.generate(prompt) : "Fix login button on mobile";
    },
    saveSuggestedTitle: async (taskId, title) => {
      saved.push({ taskId, title });
    },
    isPermanentError: opts.isPermanent ?? (() => false),
  });
  return { consumer, generated, saved };
}

const untitled: TitleableTask = { taskId: "t1", suggestedTitle: null, customTitle: null };
const ctx = { sessionId: "s1" };

describe("parseUserPrompt", () => {
  test("extracts the user prompt echo", () => {
    expect(parseUserPrompt(agentMessage("user", "fix the login button"))).toBe(
      "fix the login button",
    );
  });

  test("ignores assistant/system roles, blank prompts, and malformed payloads", () => {
    expect(parseUserPrompt(agentMessage("assistant", "on it"))).toBeUndefined();
    expect(parseUserPrompt(agentMessage("system", "note"))).toBeUndefined();
    expect(parseUserPrompt(agentMessage("user", "   "))).toBeUndefined();
    expect(
      parseUserPrompt({ idx: 1n, kind: "agent_message", payloadJson: "not json" }),
    ).toBeUndefined();
    expect(parseUserPrompt({ idx: 1n, kind: "run_started", payloadJson: "{}" })).toBeUndefined();
  });
});

describe("title consumer", () => {
  test("titles an untitled human task from the first user prompt", async () => {
    const h = harness(untitled);
    await h.consumer.handle(agentMessage("user", "fix the login button on mobile"), ctx);
    expect(h.generated).toEqual(["fix the login button on mobile"]);
    expect(h.saved).toEqual([{ taskId: "t1", title: "Fix login button on mobile" }]);
  });

  test("does not apply to sessions without a titleable task", async () => {
    const h = harness(null);
    expect(await h.consumer.appliesTo("s1")).toBe(false);
    expect(await harness(untitled).consumer.appliesTo("s1")).toBe(true);
  });

  test("skips (no model call) once a suggested title exists — replay safety", async () => {
    const h = harness({ ...untitled, suggestedTitle: "Already titled" });
    await h.consumer.handle(agentMessage("user", "another prompt"), ctx);
    expect(h.generated).toEqual([]);
    expect(h.saved).toEqual([]);
  });

  test("skips (no model call) once the user set a custom title", async () => {
    const h = harness({ ...untitled, customTitle: "My name" });
    await h.consumer.handle(agentMessage("user", "another prompt"), ctx);
    expect(h.generated).toEqual([]);
    expect(h.saved).toEqual([]);
  });

  test("ignores assistant messages without touching the DB or the model", async () => {
    let lookups = 0;
    const consumer = makeTitleConsumer({
      findTitleableTask: async () => {
        lookups += 1;
        return untitled;
      },
      generateTitle: async () => "nope",
      saveSuggestedTitle: async () => {},
      isPermanentError: () => false,
    });
    await consumer.handle(agentMessage("assistant", "working on it"), ctx);
    expect(lookups).toBe(0);
  });

  test("a permanent failure gives up on the first call — no retry loop", async () => {
    const h = harness(untitled, {
      generate: async () => {
        throw new Error("401 bad key");
      },
      isPermanent: () => true,
    });
    await h.consumer.handle(agentMessage("user", "a prompt"), ctx);
    expect(h.generated).toHaveLength(1);
    expect(h.saved).toEqual([]);
  });

  test("a transient failure throws to the pump until the 8-call cap, then gives up", async () => {
    const h = harness(untitled, {
      generate: async () => {
        throw new Error("openrouter 429");
      },
    });
    const event = agentMessage("user", "a prompt");
    for (let call = 1; call < 8; call += 1) {
      await expect(h.consumer.handle(event, ctx)).rejects.toThrow("openrouter 429");
    }
    // The 8th call exhausts the budget: swallowed, cursor advances.
    await h.consumer.handle(event, ctx);
    expect(h.generated).toHaveLength(8);
    expect(h.saved).toEqual([]);
  });

  test("a new event gets a fresh retry budget", async () => {
    const h = harness(untitled, {
      generate: async () => {
        throw new Error("flaky");
      },
    });
    const first = agentMessage("user", "a prompt");
    for (let call = 1; call < 8; call += 1) {
      await expect(h.consumer.handle(first, ctx)).rejects.toThrow("flaky");
    }
    await h.consumer.handle(first, ctx); // budget exhausted → swallowed

    // A later prompt event fails too — it must THROW (fresh budget), not
    // inherit the exhausted one and give up silently.
    const second: CuratedEvent = { ...agentMessage("user", "try again"), idx: 9n };
    await expect(h.consumer.handle(second, ctx)).rejects.toThrow("flaky");
  });
});
