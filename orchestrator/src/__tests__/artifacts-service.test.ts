/**
 * Artifact service layer: scope semantics, CASL enforcement (owner /
 * org-shared / admin), anti-enumeration, publish/update media gating,
 * and version accounting. One in-memory store fake; the same service
 * instance backs the RPC and the tool, so this is the authz matrix for
 * both surfaces.
 */

import { describe, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";

import {
  ARTIFACT_MEDIA_TYPES,
  MAX_ARTIFACT_VERSIONS,
  makeArtifactService,
  type ArtifactPullClient,
} from "../artifacts/service.ts";
import type {
  ArtifactStore,
  ArtifactVersionInput,
  ArtifactVersionRow,
  ArtifactWithVersions,
} from "../db/artifacts.ts";

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

const T0 = new Date("2026-08-03T10:00:00Z");

function makeFakeStore(): ArtifactStore & {
  rows: Map<string, ArtifactWithVersions>;
} {
  const rows = new Map<string, ArtifactWithVersions>();

  function refreshCurrent(row: ArtifactWithVersions): void {
    const current = row.versions.find((v) => v.version === row.currentVersion);
    row.mediaType = current?.mediaType ?? "application/octet-stream";
    row.sizeBytes = current?.sizeBytes ?? 0;
  }

  return {
    rows,
    async create(row, version) {
      const v1: ArtifactVersionRow = {
        ...version,
        artifactId: row.id,
        version: 1,
        createdAt: T0,
      };
      const full: ArtifactWithVersions = {
        id: row.id,
        ownerUserId: row.ownerUserId,
        title: row.title,
        fileName: row.fileName,
        visibility: "private",
        currentVersion: 1,
        createdAt: T0,
        updatedAt: T0,
        mediaType: version.mediaType,
        sizeBytes: version.sizeBytes,
        versions: [v1],
      };
      rows.set(row.id, full);
      return structuredClone(full);
    },
    async appendVersion(artifactId, version: ArtifactVersionInput, refresh) {
      const row = rows.get(artifactId);
      if (!row) return null;
      const next = row.currentVersion + 1;
      row.versions.unshift({
        ...version,
        artifactId,
        version: next,
        createdAt: T0,
      });
      row.currentVersion = next;
      if (refresh.fileName !== undefined) row.fileName = refresh.fileName;
      if (refresh.title !== undefined) row.title = refresh.title;
      refreshCurrent(row);
      return structuredClone(row);
    },
    async get(id) {
      const row = rows.get(id);
      return row ? structuredClone(row) : null;
    },
    async list(opts) {
      let all = [...rows.values()];
      if (opts.ownerUserId !== undefined) {
        all = all.filter((r) => r.ownerUserId === opts.ownerUserId);
      }
      if (opts.visibility !== undefined) {
        all = all.filter((r) => r.visibility === opts.visibility);
      }
      const totalCount = all.length;
      if (opts.pageSize > 0) {
        const start = Math.max(0, opts.page - 1) * opts.pageSize;
        all = all.slice(start, start + opts.pageSize);
      }
      return { rows: all.map((r) => structuredClone(r)), totalCount };
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
}

function makePull(mediaType = "text/html"): ArtifactPullClient & { calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    async createArtifactFromPath(req) {
      calls.push(req.path);
      return { artifactId: `coord-${calls.length}`, mediaType, sizeBytes: 42n };
    },
  };
}

const alice = { id: "alice", role: "user" };
const bob = { id: "bob", role: "user" };
const admin = { id: "root", role: "admin" };

function makeService(pullMediaType = "text/html") {
  const store = makeFakeStore();
  const pull = makePull(pullMediaType);
  let n = 0;
  const service = makeArtifactService({ store, pull, mintId: () => `a-${++n}` });
  return { service, store, pull };
}

async function code(promise: Promise<unknown>): Promise<Code | null> {
  try {
    await promise;
    return null;
  } catch (err) {
    if (err instanceof ConnectError) return err.code;
    throw err;
  }
}

// ---------------------------------------------------------------------------
// publish / update
// ---------------------------------------------------------------------------

describe("artifact service publish/update", () => {
  test("publish records owner, v1, title default from file name", async () => {
    const { service } = makeService();
    const row = await service.publish({
      sessionId: "s1",
      taskId: "t1",
      ownerUserId: alice.id,
      filePath: "/work/out/report.html",
    });
    expect(row.id).toBe("a-1");
    expect(row.ownerUserId).toBe("alice");
    expect(row.title).toBe("report.html");
    expect(row.fileName).toBe("report.html");
    expect(row.visibility).toBe("private");
    expect(row.currentVersion).toBe(1);
    expect(row.mediaType).toBe("text/html");
    expect(row.versions[0]?.coordArtifactId).toBe("coord-1");
  });

  test("publish rejects non-artifact media types", async () => {
    const { service } = makeService("image/png");
    await expect(
      code(
        service.publish({
          sessionId: "s1",
          taskId: null,
          ownerUserId: alice.id,
          filePath: "/shot.png",
        }),
      ),
    ).resolves.toBe(Code.InvalidArgument);
  });

  test("update appends a version at the same id for the owner", async () => {
    const { service } = makeService();
    const created = await service.publish({
      sessionId: "s1",
      taskId: null,
      ownerUserId: alice.id,
      filePath: "/a/report.html",
    });
    const updated = await service.update(alice, {
      artifactId: created.id,
      sessionId: "s2",
      taskId: null,
      filePath: "/b/report-v2.html",
    });
    expect(updated.currentVersion).toBe(2);
    expect(updated.fileName).toBe("report-v2.html");
    expect(updated.versions).toHaveLength(2);
    expect(updated.versions[0]?.sessionId).toBe("s2");
  });

  test("update by a non-owner is denied; admin may update", async () => {
    const { service } = makeService();
    const created = await service.publish({
      sessionId: "s1",
      taskId: null,
      ownerUserId: alice.id,
      filePath: "/report.html",
    });
    // Bob cannot even see the private artifact → NotFound.
    await expect(
      code(
        service.update(bob, {
          artifactId: created.id,
          sessionId: "s9",
          taskId: null,
          filePath: "/x.html",
        }),
      ),
    ).resolves.toBe(Code.NotFound);
    // Once org-shared, Bob can see it but still not update → PermissionDenied.
    await service.setVisibility(alice, created.id, "org");
    await expect(
      code(
        service.update(bob, {
          artifactId: created.id,
          sessionId: "s9",
          taskId: null,
          filePath: "/x.html",
        }),
      ),
    ).resolves.toBe(Code.PermissionDenied);
    const byAdmin = await service.update(admin, {
      artifactId: created.id,
      sessionId: "s9",
      taskId: null,
      filePath: "/x.html",
    });
    expect(byAdmin.currentVersion).toBe(2);
  });

  test("update stops at the version cap", async () => {
    const { service, store } = makeService();
    const created = await service.publish({
      sessionId: "s1",
      taskId: null,
      ownerUserId: alice.id,
      filePath: "/report.html",
    });
    const row = store.rows.get(created.id);
    if (!row) throw new Error("missing row");
    row.currentVersion = MAX_ARTIFACT_VERSIONS;
    await expect(
      code(
        service.update(alice, {
          artifactId: created.id,
          sessionId: "s1",
          taskId: null,
          filePath: "/report.html",
        }),
      ),
    ).resolves.toBe(Code.InvalidArgument);
  });

  test("artifact media set is exactly html + markdown", () => {
    expect([...ARTIFACT_MEDIA_TYPES].sort()).toEqual(["text/html", "text/markdown"]);
  });
});

// ---------------------------------------------------------------------------
// read surfaces
// ---------------------------------------------------------------------------

async function seed(service: ReturnType<typeof makeService>["service"]) {
  const mine = await service.publish({
    sessionId: "s1",
    taskId: null,
    ownerUserId: alice.id,
    filePath: "/alice-private.html",
  });
  const shared = await service.publish({
    sessionId: "s2",
    taskId: null,
    ownerUserId: alice.id,
    filePath: "/alice-shared.md",
  });
  await service.setVisibility(alice, shared.id, "org");
  const bobs = await service.publish({
    sessionId: "s3",
    taskId: null,
    ownerUserId: bob.id,
    filePath: "/bob-private.html",
  });
  return { mine, shared, bobs };
}

describe("artifact service read/authz matrix", () => {
  test("get: owner sees private; others 404; org-shared readable by all", async () => {
    const { service } = makeService("text/markdown");
    const { mine, shared } = await seed(service);

    await expect(service.get(alice, mine.id)).resolves.toMatchObject({ id: mine.id });
    await expect(code(service.get(bob, mine.id))).resolves.toBe(Code.NotFound);
    await expect(service.get(bob, shared.id)).resolves.toMatchObject({ id: shared.id });
    await expect(service.get(admin, mine.id)).resolves.toMatchObject({ id: mine.id });
    await expect(code(service.get(bob, "no-such-id"))).resolves.toBe(Code.NotFound);
  });

  test("list scopes: member default=mine, admin default=all, shared, all gate", async () => {
    const { service } = makeService("text/markdown");
    const { mine, shared, bobs } = await seed(service);

    const aliceDefault = await service.list(alice, { scope: "", page: 0, pageSize: 0 });
    expect(aliceDefault.rows.map((r) => r.id).sort()).toEqual([mine.id, shared.id].sort());

    const bobShared = await service.list(bob, { scope: "shared", page: 0, pageSize: 0 });
    expect(bobShared.rows.map((r) => r.id)).toEqual([shared.id]);

    const adminDefault = await service.list(admin, { scope: "", page: 0, pageSize: 0 });
    expect(adminDefault.rows).toHaveLength(3);
    expect(adminDefault.totalCount).toBe(3);

    await expect(
      code(service.list(bob, { scope: "all", page: 0, pageSize: 0 })),
    ).resolves.toBe(Code.PermissionDenied);
    await expect(
      code(service.list(bob, { scope: "everything", page: 0, pageSize: 0 })),
    ).resolves.toBe(Code.InvalidArgument);

    const bobDefault = await service.list(bob, { scope: "", page: 0, pageSize: 0 });
    expect(bobDefault.rows.map((r) => r.id)).toEqual([bobs.id]);
  });

  test("share/unshare: owner toggles; readers cannot; revoke restores 404", async () => {
    const { service } = makeService();
    const { mine } = await seed(service);

    const sharedRow = await service.setVisibility(alice, mine.id, "org");
    expect(sharedRow.visibility).toBe("org");
    await expect(service.get(bob, mine.id)).resolves.toMatchObject({ id: mine.id });

    // A reader of an org-shared artifact cannot re-share or delete it.
    await expect(code(service.setVisibility(bob, mine.id, "private"))).resolves.toBe(
      Code.PermissionDenied,
    );
    await expect(code(service.delete(bob, mine.id))).resolves.toBe(
      Code.PermissionDenied,
    );

    const revoked = await service.setVisibility(alice, mine.id, "private");
    expect(revoked.visibility).toBe("private");
    await expect(code(service.get(bob, mine.id))).resolves.toBe(Code.NotFound);

    await expect(
      code(service.setVisibility(alice, mine.id, "public")),
    ).resolves.toBe(Code.InvalidArgument);
  });

  test("delete: owner deletes; unreadable rows 404", async () => {
    const { service, store } = makeService();
    const { mine, bobs } = await seed(service);
    await expect(code(service.delete(alice, bobs.id))).resolves.toBe(Code.NotFound);
    await service.delete(alice, mine.id);
    expect(store.rows.has(mine.id)).toBe(false);
    await service.delete(admin, bobs.id);
    expect(store.rows.size, "admin can delete any").toBe(1); // alice-shared remains
  });
});
