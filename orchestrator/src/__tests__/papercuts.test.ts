import { expect, test, describe } from "bun:test";
import { ConnectError, Code, createClient, createRouterTransport } from "@connectrpc/connect";
import { timestampDate } from "@bufbuild/protobuf/wkt";

import { registerPapercuts, type PapercutDeps } from "../rpc/papercuts.ts";
import { PapercutService } from "../gen/engram/app/v1/papercut_pb.ts";
import type {
  PapercutListRow,
  PapercutStore,
} from "../db/papercuts.ts";
import type {
  ProfileInput,
  ProfileRow,
  ProfileStore,
} from "../db/profiles.ts";

const USER_ID = "papercut-user";
const CREATED_AT = new Date("2026-07-14T18:19:20.123Z");

function papercutRow(overrides: Partial<PapercutListRow> = {}): PapercutListRow {
  return {
    id: "pc-1",
    summary: "The formatter is hard to discover",
    description: "I had to search the repository to find the formatting command.",
    category: "tooling",
    severity: null,
    tags: ["developer-experience", "docs"],
    sessionId: "source-session",
    toolCallId: "tool-call-1",
    taskId: null,
    profileId: "profile-1",
    userId: USER_ID,
    archivedAt: null,
    createdAt: CREATED_AT,
    profileName: "Development",
    profileIcon: "Wrench",
    ...overrides,
  };
}

interface FakePapercutStore extends PapercutStore {
  rows: Map<string, PapercutListRow>;
  listCalls: Array<{ includeArchived: boolean; limit: number }>;
}

function makePapercuts(seed: PapercutListRow[]): FakePapercutStore {
  const rows = new Map(seed.map((row) => [row.id, row]));
  const listCalls: Array<{ includeArchived: boolean; limit: number }> = [];
  return {
    rows,
    listCalls,
    async insert() {
      throw new Error("unused");
    },
    async list(opts) {
      listCalls.push(opts);
      return [...rows.values()]
        .filter((row) => opts.includeArchived || row.archivedAt == null)
        .sort((a, b) => b.createdAt.getTime() - a.createdAt.getTime())
        .slice(0, opts.limit);
    },
    async get(id) {
      return rows.get(id) ?? null;
    },
    async setArchived(id, archived) {
      const row = rows.get(id);
      if (row) rows.set(id, { ...row, archivedAt: archived ? new Date() : null });
    },
  };
}

function profileRow(overrides: Partial<ProfileRow> = {}): ProfileRow {
  return {
    id: "profile-1",
    name: "Development",
    description: "",
    icon: "Wrench",
    imageId: "image-1",
    harness: "claude",
    model: null,
    effort: null,
    includeUserTokens: false,
    envVars: {},
    skills: [],
    capabilities: [],
    network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
    secrets: [],
    isDefault: false,
    portExposures: [],
    createdAt: new Date(0),
    updatedAt: new Date(0),
    deletedAt: null,
    ...overrides,
  };
}

function makeProfiles(seed: ProfileRow[]): ProfileStore {
  const rows = new Map(seed.map((row) => [row.id, row]));
  return {
    async list({ includeArchived }) {
      return [...rows.values()].filter((row) => includeArchived || row.deletedAt == null);
    },
    async get(id) {
      return rows.get(id) ?? null;
    },
    async getActive(id) {
      const row = rows.get(id);
      return row && row.deletedAt == null ? row : null;
    },
    async getDefault() {
      return [...rows.values()].find((row) => row.isDefault && row.deletedAt == null) ?? null;
    },
    async getByIds(ids) {
      return ids.flatMap((id) => {
        const row = rows.get(id);
        return row ? [row] : [];
      });
    },
    async create(input: ProfileInput) {
      const row = profileRow({ ...input, id: `profile-${rows.size + 1}` });
      rows.set(row.id, row);
      return row;
    },
    async update(id, input) {
      const row = rows.get(id);
      if (!row || row.deletedAt != null) return null;
      const updated = { ...row, ...input, updatedAt: new Date() };
      rows.set(id, updated);
      return updated;
    },
    async softDelete(id) {
      const row = rows.get(id);
      if (row) rows.set(id, { ...row, deletedAt: new Date() });
    },
  };
}

async function spawn(deps: Omit<PapercutDeps, "getSession">) {
  const transport = createRouterTransport((router) =>
    registerPapercuts(router, {
      getSession: async () => ({
        user: { id: USER_ID, role: "user", email: `${USER_ID}@test.invalid` },
      }),
      ...deps,
    }),
  );
  return {
    client: createClient(PapercutService, transport),
    close: async () => {},
  };
}

async function expectConnectError(
  promise: Promise<unknown>,
  code: Code,
  message?: string,
): Promise<void> {
  try {
    await promise;
    throw new Error(`expected ConnectError(${Code[code]})`);
  } catch (error) {
    if (!(error instanceof ConnectError)) throw error;
    expect(error.code).toBe(code);
    if (message != null) expect(error.rawMessage).toBe(message);
  }
}

describe("PapercutService", () => {
  test("ListPapercuts maps archived, profile, nullable strings, and timestamp", async () => {
    const store = makePapercuts([
      papercutRow({
        archivedAt: new Date("2026-07-15T00:00:00Z"),
        severity: null,
        taskId: null,
      }),
    ]);
    const server = await spawn({
      papercuts: store,
      profiles: makeProfiles([profileRow()]),
    });
    try {
      const response = await server.client.listPapercuts({ includeArchived: true });
      expect(store.listCalls).toEqual([{ includeArchived: true, limit: 500 }]);
      expect(response.papercuts).toHaveLength(1);
      const papercut = response.papercuts[0]!;
      expect(papercut.archived).toBe(true);
      expect(papercut.severity).toBe("");
      expect(papercut.taskId).toBe("");
      expect(papercut.profile).toMatchObject({
        id: "profile-1",
        name: "Development",
        icon: "Wrench",
      });
      expect(timestampDate(papercut.createdAt!)).toEqual(CREATED_AT);
    } finally {
      await server.close();
    }
  });

  test("ArchivePapercut and UnarchivePapercut round-trip", async () => {
    const store = makePapercuts([papercutRow()]);
    const server = await spawn({
      papercuts: store,
      profiles: makeProfiles([profileRow()]),
    });
    try {
      const archived = await server.client.archivePapercut({ id: "pc-1" });
      expect(archived.papercut?.archived).toBe(true);
      expect(archived.papercut?.profile).toMatchObject({
        id: "profile-1",
        name: "Development",
        icon: "Wrench",
      });
      expect(store.rows.get("pc-1")?.archivedAt).not.toBeNull();

      const unarchived = await server.client.unarchivePapercut({ id: "pc-1" });
      expect(unarchived.papercut?.archived).toBe(false);
      expect(store.rows.get("pc-1")?.archivedAt).toBeNull();
    } finally {
      await server.close();
    }
  });

  test("archive and unarchive return NotFound for an unknown papercut", async () => {
    const server = await spawn({
      papercuts: makePapercuts([]),
      profiles: makeProfiles([]),
    });
    try {
      await expectConnectError(
        server.client.archivePapercut({ id: "missing" }),
        Code.NotFound,
      );
      await expectConnectError(
        server.client.unarchivePapercut({ id: "missing" }),
        Code.NotFound,
      );
    } finally {
      await server.close();
    }
  });

});
