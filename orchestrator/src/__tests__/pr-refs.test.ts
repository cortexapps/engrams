import { describe, expect, test } from "bun:test";
import { timestampDate } from "@bufbuild/protobuf/wkt";
import {
  Code,
  ConnectError,
  createClient,
  createRouterTransport,
} from "@connectrpc/connect";

import type {
  PrRefInput,
  PrRefRow,
  PrRefStore,
} from "../db/pr-refs.ts";
import { PrRefService } from "../gen/engram/app/v1/pr_ref_pb.ts";
import { registerPrRefs, type PrRefDeps } from "../rpc/pr-refs.ts";

const OBSERVED_AT = new Date("2026-07-16T18:19:20.123Z");

function row(overrides: Partial<PrRefRow> = {}): PrRefRow {
  return {
    id: "pr-ref-1",
    repo: "openai/engrams",
    prNumber: 97,
    authoringTaskId: "task-1",
    sessionId: "session-1",
    title: "Record authored pull requests",
    url: "https://github.com/openai/engrams/pull/97",
    headBranch: "adr-0097-prref",
    baseBranch: "adr-0097-writefiles",
    observedAt: OBSERVED_AT,
    ...overrides,
  };
}

interface FakePrRefStore extends PrRefStore {
  taskCalls: string[];
  sessionCalls: string[];
}

function makeStore(rows: PrRefRow[]): FakePrRefStore {
  const taskCalls: string[] = [];
  const sessionCalls: string[] = [];
  return {
    taskCalls,
    sessionCalls,
    async upsert(_input: PrRefInput) {
      throw new Error("unused");
    },
    async listByTaskId(taskId) {
      taskCalls.push(taskId);
      return rows.filter((entry) => entry.authoringTaskId === taskId);
    },
    async listBySessionId(sessionId) {
      sessionCalls.push(sessionId);
      return rows.filter((entry) => entry.sessionId === sessionId);
    },
  };
}

function spawn(deps: Omit<PrRefDeps, "getSession">, authenticated = true) {
  const transport = createRouterTransport((router) =>
    registerPrRefs(router, {
      getSession: async () =>
        authenticated ? { user: { id: "pr-ref-user" } } : null,
      ...deps,
    }),
  );
  return createClient(PrRefService, transport);
}

async function expectConnectError(
  promise: Promise<unknown>,
  code: Code,
): Promise<void> {
  try {
    await promise;
    throw new Error(`expected ConnectError(${Code[code]})`);
  } catch (error) {
    if (!(error instanceof ConnectError)) throw error;
    expect(error.code).toBe(code);
  }
}

describe("PrRefService", () => {
  test("lists by task and maps every durable field", async () => {
    const store = makeStore([row()]);
    const client = spawn({ prRefs: store });

    const response = await client.listPrRefs({ taskId: "task-1" });

    expect(store.taskCalls).toEqual(["task-1"]);
    expect(store.sessionCalls).toEqual([]);
    expect(response.prRefs[0]).toMatchObject({
      id: "pr-ref-1",
      repo: "openai/engrams",
      prNumber: 97,
      authoringTaskId: "task-1",
      sessionId: "session-1",
      title: "Record authored pull requests",
      url: "https://github.com/openai/engrams/pull/97",
      headBranch: "adr-0097-prref",
      baseBranch: "adr-0097-writefiles",
    });
    expect(timestampDate(response.prRefs[0]!.observedAt!)).toEqual(OBSERVED_AT);
  });

  test("lists orphan PR references by observing session", async () => {
    const store = makeStore([row({ authoringTaskId: null })]);
    const client = spawn({ prRefs: store });

    const response = await client.listPrRefs({ sessionId: "session-1" });

    expect(store.sessionCalls).toEqual(["session-1"]);
    expect(response.prRefs[0]?.authoringTaskId).toBeUndefined();
  });

  test("rejects missing or ambiguous selectors", async () => {
    const client = spawn({ prRefs: makeStore([]) });

    await expectConnectError(client.listPrRefs({}), Code.InvalidArgument);
    await expectConnectError(
      client.listPrRefs({ taskId: "task-1", sessionId: "session-1" }),
      Code.InvalidArgument,
    );
  });

  test("requires an authenticated user", async () => {
    const client = spawn({ prRefs: makeStore([]) }, false);
    await expectConnectError(
      client.listPrRefs({ taskId: "task-1" }),
      Code.Unauthenticated,
    );
  });
});
