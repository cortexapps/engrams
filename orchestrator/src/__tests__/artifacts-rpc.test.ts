/**
 * ArtifactService RPC + raw-token capability + the cross-session byte
 * route. The deep authz matrix lives in artifacts-service.test.ts (the
 * shared layer); here we pin the transport mapping: identity gate,
 * record shape (created_by, raw_url, versions), token mint/verify, and
 * the byte route's dual auth (cookie CASL vs token capability) +
 * version resolution + hardened headers.
 */

import { describe, expect, test } from "bun:test";
import {
  Code,
  ConnectError,
  createClient,
  createRouterTransport,
} from "@connectrpc/connect";

import { ArtifactService } from "../gen/engram/app/v1/artifact_pb.ts";
import { registerArtifacts } from "../rpc/artifacts.ts";
import { makeArtifactsRoute } from "../routes/artifacts.ts";
import type { GetArtifactResponse, SessionsClient } from "../routes/artifacts.ts";
import {
  RAW_TOKEN_TTL_MS,
  mintRawToken,
  rawArtifactPath,
  verifyRawToken,
} from "../crypto/raw-token.ts";
import { makeArtifactService } from "../artifacts/service.ts";
import type {
  ArtifactStore,
  ArtifactVersionRow,
  ArtifactWithVersions,
} from "../db/artifacts.ts";

const KEK = Buffer.alloc(32, 7).toString("base64");
const T0 = new Date("2026-08-03T10:00:00Z");

// ---------------------------------------------------------------------------
// raw token
// ---------------------------------------------------------------------------

describe("artifact raw token", () => {
  test("mint/verify roundtrip, expiry, tamper, wrong artifact", () => {
    const token = mintRawToken("a-1", T0, KEK);
    expect(verifyRawToken("a-1", token, T0, KEK)).toBe(true);
    expect(
      verifyRawToken("a-1", token, new Date(T0.getTime() + RAW_TOKEN_TTL_MS + 1), KEK),
    ).toBe(false);
    expect(verifyRawToken("a-2", token, T0, KEK)).toBe(false);
    expect(verifyRawToken("a-1", `${token}x`, T0, KEK)).toBe(false);
    expect(verifyRawToken("a-1", "garbage", T0, KEK)).toBe(false);
    expect(verifyRawToken("a-1", "", T0, KEK)).toBe(false);
    // A tampered expiry fails the signature even if in the future.
    const [, sig] = token.split(".");
    expect(
      verifyRawToken("a-1", `${T0.getTime() + 10 * RAW_TOKEN_TTL_MS}.${sig}`, T0, KEK),
    ).toBe(false);
  });

  test("rawArtifactPath encodes the id and token", () => {
    expect(rawArtifactPath("a 1", "t.k")).toBe("/api/v1/artifacts/a%201?token=t.k");
  });
});

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

function version(
  artifactId: string,
  n: number,
  overrides: Partial<ArtifactVersionRow> = {},
): ArtifactVersionRow {
  return {
    artifactId,
    version: n,
    sessionId: `s-${n}`,
    taskId: null,
    coordArtifactId: `coord-${artifactId}-${n}`,
    mediaType: "text/html",
    sizeBytes: 10 + n,
    createdAt: T0,
    ...overrides,
  };
}

function artifactRow(
  id: string,
  owner: string | null,
  visibility: "private" | "org",
  versions: number,
): ArtifactWithVersions {
  const vs = Array.from({ length: versions }, (_, i) => version(id, versions - i));
  return {
    id,
    ownerUserId: owner,
    title: `${id} title`,
    fileName: `${id}.html`,
    visibility,
    currentVersion: versions,
    createdAt: T0,
    updatedAt: T0,
    mediaType: "text/html",
    sizeBytes: 10 + versions,
    versions: vs,
  };
}

function fixedStore(
  rows: ArtifactWithVersions[],
): ArtifactStore & { listCalls: Array<{ page: number; pageSize: number }> } {
  const byId = new Map(rows.map((r) => [r.id, r]));
  const listCalls: Array<{ page: number; pageSize: number }> = [];
  return {
    listCalls,
    async create() {
      throw new Error("unused");
    },
    async appendVersion() {
      throw new Error("unused");
    },
    async get(id) {
      return byId.get(id) ?? null;
    },
    async list(opts) {
      listCalls.push({ page: opts.page, pageSize: opts.pageSize });
      let all = [...byId.values()];
      if (opts.ownerUserId !== undefined) all = all.filter((r) => r.ownerUserId === opts.ownerUserId);
      if (opts.visibility !== undefined) all = all.filter((r) => r.visibility === opts.visibility);
      // The real store's list projection carries no version history.
      return {
        rows: all.map(({ versions: _versions, ...rest }) => rest),
        totalCount: all.length,
      };
    },
    async setVisibility(id, visibility) {
      const row = byId.get(id);
      if (!row) return null;
      row.visibility = visibility;
      return row;
    },
    async delete(id) {
      return byId.delete(id);
    },
  };
}

const noPull = {
  createArtifactFromPath: () => Promise.reject(new Error("unused")),
};

function sessionAs(user: { id: string; role?: string } | null) {
  return async () => (user ? { user } : null);
}

// ---------------------------------------------------------------------------
// RPC surface
// ---------------------------------------------------------------------------

function clientFor(
  rows: ArtifactWithVersions[],
  user: { id: string; role?: string } | null,
) {
  const store = fixedStore(rows);
  const transport = createRouterTransport((router) => {
    registerArtifacts(router, {
      getSession: sessionAs(user),
      store,
      service: makeArtifactService({ store, pull: noPull }),
      identities: {
        async getIdentity() {
          return null;
        },
        async getIdentities(ids) {
          return new Map(ids.map((id) => [id, { name: `n-${id}`, email: `${id}@x` }]));
        },
      },
      now: () => T0,
    });
  });
  return { client: createClient(ArtifactService, transport), store };
}

describe("ArtifactService rpc", () => {
  const rows = () => [
    artifactRow("a-1", "alice", "private", 2),
    artifactRow("a-2", "alice", "org", 1),
  ];

  test("unauthenticated → Unauthenticated", async () => {
    const { client } = clientFor(rows(), null);
    await expect(client.listArtifacts({ scope: "", page: 0, pageSize: 0 })).rejects.toThrow(
      ConnectError,
    );
    try {
      await client.listArtifacts({ scope: "", page: 0, pageSize: 0 });
    } catch (err) {
      expect((err as ConnectError).code).toBe(Code.Unauthenticated);
    }
  });

  test("list returns records with identity, raw_url, no versions", async () => {
    const { client } = clientFor(rows(), { id: "alice" });
    const resp = await client.listArtifacts({ scope: "", page: 0, pageSize: 0 });
    expect(resp.totalCount).toBe(2);
    const rec = resp.artifacts.find((a) => a.id === "a-1");
    if (!rec) throw new Error("missing a-1");
    expect(rec.createdBy?.name).toBe("n-alice");
    expect(rec.mediaType).toBe("text/html");
    expect(rec.currentVersion).toBe(2);
    expect(rec.versions).toHaveLength(0);
    expect(rec.rawUrl.startsWith("/api/v1/artifacts/a-1?token=")).toBe(true);
    const token = new URL(rec.rawUrl, "http://x").searchParams.get("token");
    expect(verifyRawToken("a-1", token ?? "", T0)).toBe(true);
  });

  test("an omitted page size defaults to one bounded page, never unpaginated", async () => {
    const { client, store } = clientFor(rows(), { id: "alice" });
    await client.listArtifacts({ scope: "", page: 0, pageSize: 0 });
    await client.listArtifacts({ scope: "", page: 2, pageSize: 25 });
    await client.listArtifacts({ scope: "", page: 1, pageSize: 9999 });
    expect(store.listCalls).toEqual([
      { page: 1, pageSize: 200 },
      { page: 2, pageSize: 25 },
      { page: 1, pageSize: 200 },
    ]);
  });

  test("get includes version history newest-first", async () => {
    const { client } = clientFor(rows(), { id: "alice" });
    const resp = await client.getArtifactRecord({ id: "a-1" });
    expect(resp.artifact?.versions.map((v) => v.version)).toEqual([2, 1]);
    expect(resp.artifact?.versions[0]?.sessionId).toBe("s-2");
  });

  test("member cannot read another's private artifact (NotFound)", async () => {
    const { client } = clientFor(rows(), { id: "bob" });
    try {
      await client.getArtifactRecord({ id: "a-1" });
      throw new Error("expected NotFound");
    } catch (err) {
      expect((err as ConnectError).code).toBe(Code.NotFound);
    }
    // But the org-shared one is readable.
    const shared = await client.getArtifactRecord({ id: "a-2" });
    expect(shared.artifact?.id).toBe("a-2");
  });

  test("share/unshare + delete ride the service layer", async () => {
    const { client } = clientFor(rows(), { id: "alice" });
    const shared = await client.setArtifactVisibility({ id: "a-1", visibility: "org" });
    expect(shared.artifact?.visibility).toBe("org");
    const del = await client.deleteArtifact({ id: "a-1" });
    expect(del.deleted).toBe(true);
    try {
      await client.getArtifactRecord({ id: "a-1" });
      throw new Error("expected NotFound");
    } catch (err) {
      expect((err as ConnectError).code).toBe(Code.NotFound);
    }
  });
});

// ---------------------------------------------------------------------------
// Cross-session byte route
// ---------------------------------------------------------------------------

function fakeSessions(byCoordId: Record<string, { mediaType: string; body: string }>): SessionsClient {
  return {
    async *getArtifact(req): AsyncIterable<GetArtifactResponse> {
      const entry = byCoordId[req.artifactId];
      if (!entry) {
        throw new ConnectError("artifact not found", Code.NotFound);
      }
      const bytes = new TextEncoder().encode(entry.body);
      yield {
        msg: {
          case: "metadata",
          value: {
            mediaType: entry.mediaType,
            sizeBytes: BigInt(bytes.length),
            fileName: "coordinator-name.bin",
          },
        },
      };
      yield { msg: { case: "chunk", value: bytes } };
    },
  };
}

function routeFor(
  rows: ArtifactWithVersions[],
  user: { id: string; role?: string } | null,
) {
  return makeArtifactsRoute({
    sessions: fakeSessions({
      "coord-a-1-1": { mediaType: "text/html", body: "v1" },
      "coord-a-1-2": { mediaType: "text/html", body: "v2!" },
      "coord-a-2-1": { mediaType: "text/markdown", body: "# md" },
    }),
    getSession: sessionAs(user),
    resolveOwner: async () => "unused",
    store: fixedStore(rows),
    verifyToken: (id, token) => verifyRawToken(id, token, T0, KEK),
  });
}

describe("cross-session artifact byte route", () => {
  const rows = () => {
    const md = artifactRow("a-2", "alice", "org", 1);
    md.mediaType = "text/markdown";
    for (const v of md.versions) v.mediaType = "text/markdown";
    return [artifactRow("a-1", "alice", "private", 2), md];
  };

  test("owner cookie fetch serves the current version with hardened headers", async () => {
    const res = await routeFor(rows(), { id: "alice" }).request("/api/v1/artifacts/a-1");
    expect(res.status).toBe(200);
    expect(await res.text()).toBe("v2!");
    expect(res.headers.get("content-type")).toBe("text/html");
    expect(res.headers.get("content-security-policy")).toContain("sandbox allow-scripts");
    expect(res.headers.get("x-content-type-options")).toBe("nosniff");
    // The registry's stable file name wins over the coordinator's.
    expect(res.headers.get("content-disposition")).toBe('inline; filename="a-1.html"');
    expect(res.headers.get("content-length")).toBe("3");
  });

  test("?v=N serves an old version", async () => {
    const res = await routeFor(rows(), { id: "alice" }).request("/api/v1/artifacts/a-1?v=1");
    expect(res.status).toBe(200);
    expect(await res.text()).toBe("v1");
  });

  test("unknown version and unknown id 404", async () => {
    const app = routeFor(rows(), { id: "alice" });
    expect((await app.request("/api/v1/artifacts/a-1?v=9")).status).toBe(404);
    expect((await app.request("/api/v1/artifacts/nope")).status).toBe(404);
  });

  test("anon 401; member 404 on private; org-shared readable; admin readable", async () => {
    expect((await routeFor(rows(), null).request("/api/v1/artifacts/a-1")).status).toBe(401);
    expect((await routeFor(rows(), { id: "bob" }).request("/api/v1/artifacts/a-1")).status).toBe(404);
    const shared = await routeFor(rows(), { id: "bob" }).request("/api/v1/artifacts/a-2");
    expect(shared.status).toBe(200);
    expect(shared.headers.get("content-security-policy")).toBe("sandbox");
    const admin = await routeFor(rows(), { id: "root", role: "admin" }).request(
      "/api/v1/artifacts/a-1",
    );
    expect(admin.status).toBe(200);
  });

  test("a valid token serves without a session; invalid token falls back to 401", async () => {
    const app = routeFor(rows(), null);
    const token = mintRawToken("a-1", T0, KEK);
    const ok = await app.request(`/api/v1/artifacts/a-1?token=${encodeURIComponent(token)}`);
    expect(ok.status).toBe(200);
    expect(await ok.text()).toBe("v2!");

    const expired = mintRawToken("a-1", new Date(T0.getTime() - 2 * RAW_TOKEN_TTL_MS), KEK);
    expect(
      (await app.request(`/api/v1/artifacts/a-1?token=${encodeURIComponent(expired)}`)).status,
    ).toBe(401);
    // A token for one artifact does not open another.
    expect(
      (await app.request(`/api/v1/artifacts/a-2?token=${encodeURIComponent(token)}`)).status,
    ).toBe(401);
  });
});
