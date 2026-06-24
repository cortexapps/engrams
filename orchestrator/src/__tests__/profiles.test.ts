import { expect, test, describe, beforeAll, afterAll } from "bun:test";
import { ConnectError, Code, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import { Hono } from "hono";
import type { AddressInfo } from "node:net";

import { buildServer } from "../server.ts";
import { registerProfiles } from "../rpc/profiles.ts";
import type { ProfileDeps, ImagesClient, GetSession } from "../rpc/profiles.ts";
import type { ProfileRow, ProfileStore, ProfileInput } from "../db/profiles.ts";
import { ProfileService } from "../gen/engram/app/v1/profile_pb.ts";
import type { MountCatalogClient } from "../skills/catalog.ts";

/** Fake catalog whose live uploaded skills are `uploadedNames` (builtins are implicit). */
const fakeCatalog = (uploadedNames: string[] = []): MountCatalogClient => ({
  async listSkills() {
    return {
      skills: uploadedNames.map((name, i) => ({
        id: `s${i}`,
        owner: "x",
        name,
        description: "",
        sha256: "h",
        sizeBytes: 0n,
        createdAt: "",
      })),
    };
  },
  async getSkill() {
    return {};
  },
  async registerSkill() {
    throw new Error("unused");
  },
  async deleteSkill() {
    return { deleted: false };
  },
});

function makeGetSession(userId: string | null, role: "user" | "admin" = "user"): GetSession {
  return async () => (userId ? { user: { id: userId, role, email: `${userId}@t.invalid` } } : null);
}

const fakeImages = (ids: string[]): ImagesClient => ({
  async listEnabledImages() {
    return { images: ids.map((id) => ({ id, imageUri: `registry/${id}:latest` })) };
  },
});

/** In-memory ProfileStore for the authz/field-filter matrix (no DB). */
function makeFakeStore(seed: ProfileRow[] = []): ProfileStore {
  const rows = new Map<string, ProfileRow>(seed.map((r) => [r.id, r]));
  let n = 0;
  const mk = (id: string, input: ProfileInput): ProfileRow => ({
    id, ...input, createdAt: new Date(0), updatedAt: new Date(0), deletedAt: null,
  });
  return {
    async list({ includeArchived }) {
      return [...rows.values()]
        .filter((r) => includeArchived || r.deletedAt == null)
        .sort((a, b) => a.name.localeCompare(b.name));
    },
    async get(id) { return rows.get(id) ?? null; },
    async getActive(id) { const r = rows.get(id); return r && r.deletedAt == null ? r : null; },
    async getByIds(ids) { return ids.map((i) => rows.get(i)).filter(Boolean) as ProfileRow[]; },
    async create(input) { const id = `p${n++}`; const r = mk(id, input); rows.set(id, r); return r; },
    async update(id, input) {
      const ex = rows.get(id); if (!ex || ex.deletedAt != null) return null;
      const r = { ...ex, ...input, updatedAt: new Date(0) }; rows.set(id, r); return r;
    },
    async softDelete(id) { const r = rows.get(id); if (r) rows.set(id, { ...r, deletedAt: new Date(0) }); },
  };
}

async function spawn(deps: ProfileDeps) {
  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));
  const srv = buildServer(app, (router) => registerProfiles(router, deps));
  const url = await new Promise<string>((res) =>
    srv.listen(0, "127.0.0.1", () => res(`http://127.0.0.1:${(srv.address() as AddressInfo).port}`)),
  );
  return {
    client: createClient(ProfileService, createConnectTransport({ baseUrl: `${url}/rpc`, httpVersion: "1.1" })),
    close: () => new Promise<void>((res, rej) => srv.close((e) => (e ? rej(e) : res()))),
  };
}

async function expectErr(p: Promise<unknown>, code: Code) {
  try { await p; throw new Error(`expected ${Code[code]}`); }
  catch (e) { if (!(e instanceof ConnectError)) throw e; expect(e.code).toBe(code); }
}

const archived: ProfileRow = {
  id: "arch", name: "Archived", description: "", icon: "Bot", imageId: "img-1",
  includeUserTokens: false, envVars: { K: "V" }, skills: [], capabilities: [], createdAt: new Date(0), updatedAt: new Date(0),
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] }, secrets: [],
  deletedAt: new Date(0),
};
const active: ProfileRow = { ...archived, id: "act", name: "Active", deletedAt: null };

describe("ProfileService — auth + field filtering", () => {
  test("anon ListProfiles → Unauthenticated", async () => {
    const s = await spawn({ getSession: makeGetSession(null), store: makeFakeStore(), images: fakeImages([]) });
    try { await expectErr(s.client.listProfiles({}), Code.Unauthenticated); } finally { await s.close(); }
  });

  test("member: active only, env_vars stripped, include_archived ignored", async () => {
    const s = await spawn({
      getSession: makeGetSession("m"), store: makeFakeStore([active, archived]), images: fakeImages(["img-1"]),
    });
    try {
      const r = await s.client.listProfiles({ includeArchived: true });
      expect(r.profiles.map((p) => p.id)).toEqual(["act"]); // archived hidden, include ignored
      expect(r.profiles[0]!.envVars).toEqual({}); // stripped
      expect(r.profiles[0]!.includeUserTokens).toBe(false); // still visible
    } finally { await s.close(); }
  });

  test("admin: include_archived shows archived + env_vars present", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore([active, archived]), images: fakeImages(["img-1"]),
    });
    try {
      const r = await s.client.listProfiles({ includeArchived: true });
      expect(r.profiles.map((p) => p.id).sort()).toEqual(["act", "arch"]);
      expect(r.profiles.find((p) => p.id === "act")!.envVars).toEqual({ K: "V" });
    } finally { await s.close(); }
  });

  test("member CreateProfile → PermissionDenied", async () => {
    const s = await spawn({ getSession: makeGetSession("m"), store: makeFakeStore(), images: fakeImages(["img-1"]) });
    try {
      await expectErr(
        s.client.createProfile({ name: "x", description: "", icon: "Bot", imageId: "img-1", includeUserTokens: false, envVars: {} }),
        Code.PermissionDenied,
      );
    } finally { await s.close(); }
  });

  test("admin CreateProfile with unknown image_id → InvalidArgument", async () => {
    const s = await spawn({ getSession: makeGetSession("a", "admin"), store: makeFakeStore(), images: fakeImages(["img-1"]) });
    try {
      await expectErr(
        s.client.createProfile({ name: "x", description: "", icon: "Bot", imageId: "nope", includeUserTokens: false, envVars: {} }),
        Code.InvalidArgument,
      );
    } finally { await s.close(); }
  });

  test("admin CreateProfile happy path returns archived=false + env_vars", async () => {
    const s = await spawn({ getSession: makeGetSession("a", "admin"), store: makeFakeStore(), images: fakeImages(["img-1"]) });
    try {
      const r = await s.client.createProfile({
        name: "New", description: "d", icon: "Rocket", imageId: "img-1", includeUserTokens: true, envVars: { ANTHROPIC_MODEL: "claude-opus-4-8" },
      });
      expect(r.profile!.archived).toBe(false);
      expect(r.profile!.envVars).toEqual({ ANTHROPIC_MODEL: "claude-opus-4-8" });
      expect(r.profile!.includeUserTokens).toBe(true);
    } finally { await s.close(); }
  });

  // ADR 0055 P2: skills validated against builtins ∪ the upload catalog.
  test("admin CreateProfile with an unknown skill → InvalidArgument", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore(),
      images: fakeImages(["img-1"]), mountCatalog: fakeCatalog(["my-linter"]),
    });
    try {
      await expectErr(
        s.client.createProfile({ name: "x", description: "", icon: "Bot", imageId: "img-1", includeUserTokens: false, envVars: {}, skills: ["nope"] }),
        Code.InvalidArgument,
      );
    } finally { await s.close(); }
  });

  test("admin CreateProfile accepts a builtin + an uploaded skill", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore(),
      images: fakeImages(["img-1"]), mountCatalog: fakeCatalog(["my-linter"]),
    });
    try {
      const r = await s.client.createProfile({
        name: "Skilled", description: "", icon: "Bot", imageId: "img-1", includeUserTokens: false, envVars: {}, skills: ["skills", "my-linter"],
      });
      expect(r.profile!.skills).toEqual(["skills", "my-linter"]);
    } finally { await s.close(); }
  });

  test("admin CreateProfile with a malformed capability → InvalidArgument (ADR 0056)", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore(),
      images: fakeImages(["img-1"]), mountCatalog: fakeCatalog([]),
    });
    try {
      await expectErr(
        s.client.createProfile({ name: "x", description: "", icon: "Bot", imageId: "img-1", includeUserTokens: false, envVars: {}, capabilities: ["github"] }),
        Code.InvalidArgument,
      );
    } finally { await s.close(); }
  });

  test("admin CreateProfile accepts + returns well-formed capabilities (ADR 0056)", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore(),
      images: fakeImages(["img-1"]), mountCatalog: fakeCatalog([]),
    });
    try {
      const r = await s.client.createProfile({
        name: "Capable", description: "", icon: "Bot", imageId: "img-1", includeUserTokens: false, envVars: {},
        capabilities: ["github:issues:write", "datadog:read@idx-1"],
      });
      expect(r.profile!.capabilities).toEqual(["github:issues:write", "datadog:read@idx-1"]);
    } finally { await s.close(); }
  });
});
