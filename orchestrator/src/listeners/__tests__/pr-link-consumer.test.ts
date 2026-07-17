import { describe, expect, test } from "bun:test";

import type { CuratedEvent } from "../../control-plane/session-events.ts";
import type {
  PrRefInput,
  PrRefRow,
  PrRefStore,
} from "../../db/pr-refs.ts";
import { makePrLinkConsumer } from "../pr-link-consumer.ts";

function integrationAsset(payload: unknown): CuratedEvent {
  return {
    idx: 17n,
    kind: "integration_asset",
    payloadJson: JSON.stringify(payload),
  };
}

function pullRequestAsset(): CuratedEvent {
  return integrationAsset({
    provider: "forge",
    asset_kind: "pull_request",
    surface: "asset",
    data: {
      repo: "openai/engrams",
      number: 97,
      title: "Record authored pull requests",
      head_branch: "adr-0097-prref",
      base_branch: "adr-0097-writefiles",
    },
    fetchable: {
      kind: "external",
      url: "https://github.com/openai/engrams/pull/97",
    },
    at: "2026-07-16T18:19:20.123Z",
  });
}

function prRefRecorder() {
  const upserts: PrRefInput[] = [];
  const rows = new Map<string, PrRefRow>();
  const store: PrRefStore = {
    async upsert(input) {
      upserts.push(input);
      const key = `${input.repo}#${input.prNumber}`;
      const existing = rows.get(key);
      const id = existing?.id ?? `pr-ref-${rows.size + 1}`;
      rows.set(key, {
        id,
        ...input,
        authoringTaskId: existing?.authoringTaskId ?? input.authoringTaskId,
      });
      return id;
    },
    async listByTaskId(taskId) {
      return [...rows.values()].filter((row) => row.authoringTaskId === taskId);
    },
    async listBySessionId(sessionId) {
      return [...rows.values()].filter((row) => row.sessionId === sessionId);
    },
  };
  return { store, rows, upserts };
}

describe("PR-link consumer", () => {
  test("projects a pull-request asset with its task link and exact payload fields", async () => {
    const recorder = prRefRecorder();
    const consumer = makePrLinkConsumer({
      prRefs: recorder.store,
      findTaskId: async () => "task-1",
    });

    await consumer.handle(pullRequestAsset(), { sessionId: "session-1" });

    expect(recorder.upserts).toEqual([
      {
        repo: "openai/engrams",
        prNumber: 97,
        authoringTaskId: "task-1",
        sessionId: "session-1",
        title: "Record authored pull requests",
        url: "https://github.com/openai/engrams/pull/97",
        headBranch: "adr-0097-prref",
        baseBranch: "adr-0097-writefiles",
        observedAt: new Date("2026-07-16T18:19:20.123Z"),
      },
    ]);
  });

  test("records the link even when fetchable is null and decorative fields are absent", async () => {
    // The coordinator marks `fetchable` as Option and `data` comes from the
    // egress proxy — only repo+number are guaranteed. The durable task→PR
    // link must never be dropped over missing decoration.
    const recorder = prRefRecorder();
    const consumer = makePrLinkConsumer({
      prRefs: recorder.store,
      findTaskId: async () => "task-1",
    });

    await consumer.handle(
      integrationAsset({
        provider: "forge",
        asset_kind: "pull_request",
        surface: "asset",
        data: { repo: "openai/engrams", number: 98 },
        fetchable: null,
        at: "2026-07-16T18:19:20.123Z",
      }),
      { sessionId: "session-1" },
    );

    expect(recorder.upserts).toEqual([
      {
        repo: "openai/engrams",
        prNumber: 98,
        authoringTaskId: "task-1",
        sessionId: "session-1",
        title: "",
        url: "",
        headBranch: "",
        baseBranch: "",
        observedAt: new Date("2026-07-16T18:19:20.123Z"),
      },
    ]);
  });

  test("ignores non-PR integration assets", async () => {
    const recorder = prRefRecorder();
    const consumer = makePrLinkConsumer({
      prRefs: recorder.store,
      findTaskId: async () => "task-1",
    });

    await consumer.handle(
      integrationAsset({ asset_kind: "issue", data: {} }),
      { sessionId: "session-1" },
    );

    expect(recorder.upserts).toEqual([]);
  });

  test("logs and skips malformed JSON without throwing", async () => {
    const recorder = prRefRecorder();
    const consumer = makePrLinkConsumer({
      prRefs: recorder.store,
      findTaskId: async () => "task-1",
    });
    const malformed: CuratedEvent = {
      idx: 18n,
      kind: "integration_asset",
      payloadJson: "{not-json",
    };

    await expect(
      consumer.handle(malformed, { sessionId: "session-1" }),
    ).resolves.toBeUndefined();
    expect(recorder.upserts).toEqual([]);
  });

  test("redelivery upserts the same PR and remains one durable row", async () => {
    const recorder = prRefRecorder();
    const consumer = makePrLinkConsumer({
      prRefs: recorder.store,
      findTaskId: async () => null,
    });
    const event = pullRequestAsset();

    await consumer.handle(event, { sessionId: "orphan-session" });
    await consumer.handle(event, { sessionId: "orphan-session" });

    expect(recorder.upserts).toHaveLength(2);
    expect(recorder.rows.size).toBe(1);
    expect([...recorder.rows.values()][0]?.authoringTaskId).toBeNull();
  });
});
