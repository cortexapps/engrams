/**
 * Repo autodiscovery flow: boot → one bounded exec → parse → teardown.
 * The invariant under test is teardown: the probe session AND its system task
 * row are removed on success, on scan failure, and on non-zero exit — a
 * discovery can never leave a session running or a task behind.
 */

import { describe, expect, test } from "bun:test";
import type { ProfileRow } from "../../db/profiles.ts";
import { discoverProfileRepos, DiscoverReposError, type DiscoverReposDeps } from "../profile-discover.ts";
import { DISCOVER_COMMAND } from "../profile-repos.ts";

const PROFILE = { id: "prof-1", name: "Backend API" } as ProfileRow;

interface FakeWorld {
  deps: DiscoverReposDeps;
  taskInserts: Record<string, unknown>[];
  taskDeletes: number;
  deletedSessions: string[];
  execCommands: string[];
}

function world(options: {
  stdout?: string;
  exitStatus?: number;
  execError?: boolean;
  createError?: boolean;
} = {}): FakeWorld {
  const taskInserts: Record<string, unknown>[] = [];
  const deletedSessions: string[] = [];
  const execCommands: string[] = [];
  const w: FakeWorld = {
    taskInserts,
    taskDeletes: 0,
    deletedSessions,
    execCommands,
    deps: {
      profiles: { getActive: async (id) => (id === PROFILE.id ? PROFILE : null) },
      db: {
        insert: () => ({ values: async (v: Record<string, unknown>) => void taskInserts.push(v) }),
        delete: () => ({ where: async () => void (w.taskDeletes += 1) }),
      } as unknown as DiscoverReposDeps["db"],
      // Unused by the flow when createSession is injected:
      images: undefined as never,
      connectors: undefined as never,
      harnessCatalog: undefined as never,
      secrets: { get: async () => null, getAll: async () => ({}) },
      createSession: async (_deps, params) => {
        if (options.createError) throw new Error("no capacity");
        expect(params).toMatchObject({ profileId: PROFILE.id, role: "primary" });
        return { sessionId: "probe-session" };
      },
      sessions: {
        createSession: async () => ({ sessionId: "probe-session" }),
        deleteSession: async ({ sessionId }: { sessionId: string }) => {
          deletedSessions.push(sessionId);
          return {};
        },
        async *exec(req: { command: string; execId?: string }) {
          execCommands.push(req.command);
          yield { event: { case: "started" as const, value: { execId: req.execId ?? "e" } } };
          if (options.execError) {
            throw Object.assign(new Error("boom"), { name: "ConnectError" });
          }
          if (options.stdout) {
            yield {
              event: { case: "stdout" as const, value: new TextEncoder().encode(options.stdout) },
            };
          }
          yield {
            event: { case: "exit" as const, value: { exitStatus: options.exitStatus ?? 0 } },
          };
        },
        cancelExec: async () => ({}),
      } as unknown as DiscoverReposDeps["sessions"],
      execRuntime: {
        nowMs: () => 0,
        sleep: async () => {},
        scheduleDeadline: () => () => {},
      },
    },
  };
  return w;
}

describe("discoverProfileRepos", () => {
  test("runs THE bounded command, parses, and tears down session + task", async () => {
    const w = world({
      stdout: "REPO /workspace/engrams\nREMOTE origin\thttps://github.com/cortexapps/engrams.git (fetch)\n",
    });
    const repos = await discoverProfileRepos(w.deps, PROFILE.id);

    expect(w.execCommands).toEqual([DISCOVER_COMMAND]);
    expect(repos).toEqual([
      {
        path: "/workspace/engrams",
        remotes: [
          {
            name: "origin",
            url: "https://github.com/cortexapps/engrams.git",
            parsed: { host: "github.com", owner: "cortexapps", name: "engrams" },
          },
        ],
      },
    ]);
    // The probe is a SYSTEM task (no owner) and leaves nothing behind.
    expect(w.taskInserts[0]).toMatchObject({ type: "profile_discover", createdByUserId: null });
    expect(w.deletedSessions).toEqual(["probe-session"]);
    expect(w.taskDeletes).toBe(1);
  });

  test("a non-zero scan exit throws but still tears down", async () => {
    const w = world({ exitStatus: 7, stdout: "partial" });
    await expect(discoverProfileRepos(w.deps, PROFILE.id)).rejects.toBeInstanceOf(DiscoverReposError);
    expect(w.deletedSessions).toEqual(["probe-session"]);
    expect(w.taskDeletes).toBe(1);
  });

  test("a session-create failure removes the task row (no session to delete)", async () => {
    const w = world({ createError: true });
    await expect(discoverProfileRepos(w.deps, PROFILE.id)).rejects.toThrow("no capacity");
    expect(w.deletedSessions).toEqual([]);
    expect(w.taskDeletes).toBe(1);
  });

  test("an unknown profile throws before any task is created", async () => {
    const w = world();
    await expect(discoverProfileRepos(w.deps, "ghost")).rejects.toThrow("profile not found");
    expect(w.taskInserts).toEqual([]);
    expect(w.taskDeletes).toBe(0);
  });
});
