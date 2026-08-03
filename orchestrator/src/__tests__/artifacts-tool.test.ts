/**
 * The Artifact tool: action dispatch through executeToolCall onto the
 * shared service layer. Pins the ADR 0089 integration (context
 * resolution supplies the session owner, capability-free registration,
 * arg validation) and the tool-visible behavior per action; the deep
 * authz matrix lives in artifacts-service.test.ts.
 */

import { describe, expect, test } from "bun:test";

import { makeArtifactService } from "../artifacts/service.ts";
import type {
  ArtifactStore,
  ArtifactVersionInput,
  ArtifactWithVersions,
} from "../db/artifacts.ts";
import { registerBuiltinTools } from "../tools/builtin.ts";
import { executeToolCall, type ToolExecDeps } from "../tools/exec.ts";
import { createToolRegistry } from "../tools/registry.ts";

const T0 = new Date("2026-08-03T10:00:00Z");

function makeFakeStore() {
  const rows = new Map<string, ArtifactWithVersions>();
  const store: ArtifactStore = {
    async create(row, v) {
      const full: ArtifactWithVersions = {
        id: row.id,
        ownerUserId: row.ownerUserId,
        title: row.title,
        fileName: row.fileName,
        visibility: "private",
        currentVersion: 1,
        createdAt: T0,
        updatedAt: T0,
        mediaType: v.mediaType,
        sizeBytes: v.sizeBytes,
        versions: [{ ...v, artifactId: row.id, version: 1, createdAt: T0 }],
      };
      rows.set(row.id, full);
      return structuredClone(full);
    },
    async appendVersion(id, v: ArtifactVersionInput, refresh) {
      const row = rows.get(id);
      if (!row) return null;
      row.currentVersion += 1;
      row.versions.unshift({
        ...v,
        artifactId: id,
        version: row.currentVersion,
        createdAt: T0,
      });
      row.mediaType = v.mediaType;
      row.sizeBytes = v.sizeBytes;
      if (refresh.fileName !== undefined) row.fileName = refresh.fileName;
      if (refresh.title !== undefined) row.title = refresh.title;
      return structuredClone(row);
    },
    async get(id) {
      const row = rows.get(id);
      return row ? structuredClone(row) : null;
    },
    async list(opts) {
      let all = [...rows.values()];
      if (opts.ownerUserId !== undefined) all = all.filter((r) => r.ownerUserId === opts.ownerUserId);
      if (opts.visibility !== undefined) all = all.filter((r) => r.visibility === opts.visibility);
      return { rows: all.map((r) => structuredClone(r)), totalCount: all.length };
    },
    async setVisibility(id, visibility) {
      const row = rows.get(id);
      if (!row) return null;
      row.visibility = visibility;
      return structuredClone(row);
    },
    async delete(id) {
      return rows.delete(id);
    },
  };
  return { rows, store };
}

interface Harness {
  exec(userId: string | undefined, args: unknown): Promise<unknown>;
  pullCalls: Array<{ sessionId: string; path: string; suppressEvent?: boolean }>;
}

function makeHarness(pullMediaType = "text/html"): Harness {
  const { store } = makeFakeStore();
  const pullCalls: Harness["pullCalls"] = [];
  let coordN = 0;
  let mintN = 0;
  const service = makeArtifactService({
    store,
    pull: {
      async createArtifactFromPath(req) {
        pullCalls.push(req);
        return { artifactId: `coord-${++coordN}`, mediaType: pullMediaType, sizeBytes: 5 };
      },
    },
    mintId: () => `a-${++mintN}`,
  });
  const registry = createToolRegistry();
  registerBuiltinTools(registry, {
    artifacts: service,
    now: () => T0,
    // Fake byte stream: serves "content of <coordArtifactId>" so a get
    // with include_content proves which VERSION's bytes were read.
    artifactStream: {
      async *getArtifact(req) {
        const body = new TextEncoder().encode(`content of ${req.artifactId}`);
        yield {
          msg: {
            case: "metadata" as const,
            value: {
              mediaType: "text/html",
              sizeBytes: BigInt(body.length),
              fileName: "x.html",
            },
          },
        };
        yield { msg: { case: "chunk" as const, value: body } };
      },
    },
  });

  const deps: ToolExecDeps = {
    registry,
    resolveContext: async (sessionId) => ({
      sessionId,
      taskId: "t1",
      capabilities: [],
      ...(currentUser !== undefined ? { userId: currentUser } : {}),
    }),
    pendingCalls: {
      recordRequested: async () => {},
      markSubmitted: async () => {},
      markCompleted: async () => {},
      listSessionIdsWithPendingSessionCalls: async () => [],
      find: async () => null,
      listUnsubmittedSessionCallsBefore: async () => [],
    },
    completer: {
      completeToolCall: async () => ({}),
    } as unknown as ToolExecDeps["completer"],
    now: () => T0,
    nowMs: () => T0.getTime(),
  };

  let currentUser: string | undefined;
  let callN = 0;
  return {
    pullCalls,
    async exec(userId, args) {
      currentUser = userId;
      const outcome = await executeToolCall(
        {
          sessionId: "s1",
          toolCallId: `call-${++callN}`,
          toolName: "Artifact",
          argsJson: JSON.stringify(args),
        },
        deps,
      );
      if (outcome.kind !== "submit") throw new Error("expected submit");
      return outcome.result;
    },
  };
}

type ToolResult = {
  error?: string;
  artifact?: Record<string, unknown>;
  artifacts?: Array<Record<string, unknown>>;
  total_count?: number;
};

describe("Artifact tool", () => {
  test("publish returns the artifact with stable + raw urls and suppresses the share event", async () => {
    const h = makeHarness();
    const result = (await h.exec("alice", {
      action: "publish",
      file_path: "/work/report.html",
      title: "Q3 report",
    })) as ToolResult;
    expect(result.error).toBeUndefined();
    const artifact = result.artifact;
    if (!artifact) throw new Error("missing artifact");
    expect(artifact["id"]).toBe("a-1");
    expect(artifact["title"]).toBe("Q3 report");
    expect(artifact["file_name"]).toBe("report.html");
    expect(artifact["current_version"]).toBe(1);
    expect(String(artifact["url"])).toContain("/artifacts/a-1");
    expect(String(artifact["raw_url"])).toContain("/api/v1/artifacts/a-1?token=");
    expect(h.pullCalls).toEqual([
      { sessionId: "s1", path: "/work/report.html", suppressEvent: true },
    ]);
  });

  test("publish rejects non-artifact media types with a clear error", async () => {
    const h = makeHarness("image/png");
    const result = (await h.exec("alice", {
      action: "publish",
      file_path: "/shot.png",
    })) as ToolResult;
    expect(result.error).toContain("only HTML and Markdown");
  });

  test("update bumps the version at the same id; non-owner is denied", async () => {
    const h = makeHarness();
    await h.exec("alice", { action: "publish", file_path: "/report.html" });
    const updated = (await h.exec("alice", {
      action: "update",
      artifact_id: "a-1",
      file_path: "/report-v2.html",
    })) as ToolResult;
    expect(updated.artifact?.["current_version"]).toBe(2);
    expect(updated.artifact?.["file_name"]).toBe("report-v2.html");

    const denied = (await h.exec("bob", {
      action: "update",
      artifact_id: "a-1",
      file_path: "/x.html",
    })) as ToolResult;
    expect(denied.error).toContain("not found");
  });

  test("list/get/share/unshare round-trip", async () => {
    const h = makeHarness();
    await h.exec("alice", { action: "publish", file_path: "/one.md" });

    const listed = (await h.exec("alice", { action: "list" })) as ToolResult;
    expect(listed.total_count).toBe(1);
    expect(listed.artifacts?.[0]?.["id"]).toBe("a-1");

    // Bob sees nothing under "mine" and nothing shared yet.
    const bobMine = (await h.exec("bob", { action: "list" })) as ToolResult;
    expect(bobMine.total_count).toBe(0);
    const bobShared = (await h.exec("bob", { action: "list", scope: "shared" })) as ToolResult;
    expect(bobShared.total_count).toBe(0);

    const shared = (await h.exec("alice", { action: "share", artifact_id: "a-1" })) as ToolResult;
    expect(shared.artifact?.["visibility"]).toBe("org");
    const bobGets = (await h.exec("bob", { action: "get", artifact_id: "a-1" })) as ToolResult;
    expect(bobGets.artifact?.["id"]).toBe("a-1");

    const unshared = (await h.exec("alice", { action: "unshare", artifact_id: "a-1" })) as ToolResult;
    expect(unshared.artifact?.["visibility"]).toBe("private");
    const bobDenied = (await h.exec("bob", { action: "get", artifact_id: "a-1" })) as ToolResult;
    expect(bobDenied.error).toContain("not found");
  });

  test("get with include_content returns a chosen version's source", async () => {
    const h = makeHarness();
    await h.exec("alice", { action: "publish", file_path: "/r.html" }); // coord-1
    await h.exec("alice", { action: "update", artifact_id: "a-1", file_path: "/r.html" }); // coord-2

    const current = (await h.exec("alice", {
      action: "get",
      artifact_id: "a-1",
      include_content: true,
    })) as ToolResult & { content?: string; content_version?: number };
    expect(current.content).toBe("content of coord-2");
    expect(current.content_version).toBe(2);

    const v1 = (await h.exec("alice", {
      action: "get",
      artifact_id: "a-1",
      version: 1,
      include_content: true,
    })) as ToolResult & { content?: string; content_version?: number };
    expect(v1.content).toBe("content of coord-1");
    expect(v1.content_version).toBe(1);

    const missing = (await h.exec("alice", {
      action: "get",
      artifact_id: "a-1",
      version: 9,
      include_content: true,
    })) as ToolResult;
    expect(missing.error).toContain("no version 9");

    // Without the flag the result stays metadata-only.
    const plain = (await h.exec("alice", {
      action: "get",
      artifact_id: "a-1",
    })) as ToolResult & { content?: string };
    expect(plain.content).toBeUndefined();
  });

  test("a session with no owning user cannot manage artifacts", async () => {
    const h = makeHarness();
    const result = (await h.exec(undefined, {
      action: "publish",
      file_path: "/x.html",
    })) as ToolResult;
    expect(result.error).toContain("no owning user");
  });

  test("invalid args are rejected with the offending field named", async () => {
    const h = makeHarness();
    const result = (await h.exec("alice", { action: "publish" })) as ToolResult;
    expect(result.error).toContain("file_path");
  });
});
