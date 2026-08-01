import { expect, test, describe, beforeAll, afterAll } from "bun:test";
import { ConnectError, Code, createClient, createRouterTransport } from "@connectrpc/connect";
import { create } from "@bufbuild/protobuf";

import { registerProfiles } from "../rpc/profiles.ts";
import type { ProfileDeps, ImagesClient, GetSession, HarnessCatalogClient } from "../rpc/profiles.ts";
import type { ProfileRow, ProfileStore, ProfileInput } from "../db/profiles.ts";
import { ProfileIntegrationGrantSchema, ProfileService } from "../gen/engram/app/v1/profile_pb.ts";
import type { MountCatalogClient } from "../skills/catalog.ts";
import { PR_REVIEW_CAPABILITY } from "../tools/review.ts";
import { capabilityGrant } from "../integrations/grants.ts";
import type { IntegrationConnectionStore } from "../db/integration-connections.ts";

const protoGrant = (capability: string) =>
  create(ProfileIntegrationGrantSchema, capabilityGrant(
    capability,
    `default-${capability.slice(0, capability.indexOf(":"))}`,
  ));

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

/** Minimal catalog so `assertHarnessValid` can resolve the required harness +
 *  validate model/effort ids (ADR 0063 — a profile always names a harness). */
const fakeHarnessCatalog = (): HarnessCatalogClient => ({
  async listHarnesses() {
    return {
      harnesses: [
        {
          name: "claude",
          descriptor: {
            models: [{ id: "opus", default: true }, { id: "sonnet", default: false }],
            effort: [{ id: "high", default: true }],
          },
        },
      ],
    };
  },
});

/** In-memory ProfileStore for the authz/field-filter matrix (no DB). */
function makeFakeStore(seed: ProfileRow[] = []): ProfileStore {
  const rows = new Map<string, ProfileRow>(seed.map((r) => [r.id, r]));
  let n = 0;
  const mk = (id: string, input: ProfileInput, designation: string | null = null): ProfileRow => ({
    id, ...input, designation, createdAt: new Date(0), updatedAt: new Date(0), deletedAt: null,
  });
  return {
    async list({ includeArchived }) {
      return [...rows.values()]
        .filter((r) => includeArchived || r.deletedAt == null)
        .sort((a, b) => a.name.localeCompare(b.name));
    },
    async get(id) { return rows.get(id) ?? null; },
    async getActive(id) { const r = rows.get(id); return r && r.deletedAt == null ? r : null; },
    async getDefault() { return [...rows.values()].find((r) => r.isDefault && r.deletedAt == null) ?? null; },
    async getByDesignation(designation) {
      return [...rows.values()].find((r) => r.designation === designation && r.deletedAt == null) ?? null;
    },
    async getByIds(ids) { return ids.map((i) => rows.get(i)).filter(Boolean) as ProfileRow[]; },
    async create(input, designation) { const id = `p${n++}`; const r = mk(id, input, designation); rows.set(id, r); return r; },
    async setDesignation(id, designation) {
      if (designation !== null) {
        for (const [otherId, row] of rows) {
          if (otherId !== id && row.designation === designation) {
            rows.set(otherId, { ...row, designation: null, updatedAt: new Date(0) });
          }
        }
      }
      const row = rows.get(id);
      if (row) rows.set(id, { ...row, designation, updatedAt: new Date(0) });
    },
    async update(id, input) {
      const ex = rows.get(id); if (!ex || ex.deletedAt != null) return null;
      const r = { ...ex, ...input, updatedAt: new Date(0) }; rows.set(id, r); return r;
    },
    async softDelete(id) { const r = rows.get(id); if (r) rows.set(id, { ...r, deletedAt: new Date(0) }); },
  };
}

async function spawn(deps: ProfileDeps) {
  // ADR 0063: a profile always validates its harness against the catalog —
  // default to the fake so tests don't reach the live (coord) client. Specific
  // tests can still override.
  const connections: IntegrationConnectionStore = {
    list: async () => [],
    get: async (id) => id.startsWith("default-") ? {
      id,
      alias: id,
      provider: id.slice(8),
      displayName: id,
      isDefault: true,
      config: {},
      enabled: true,
      testedAt: new Date(0),
      createdAt: new Date(0),
      updatedAt: new Date(0),
    } : null,
    getDefault: async (provider) => connections.get(`default-${provider}`),
    create: async () => { throw new Error("unused"); },
    update: async () => { throw new Error("unused"); },
    delete: async () => { throw new Error("unused"); },
    markTested: async () => { throw new Error("unused"); },
    setEnabled: async () => { throw new Error("unused"); },
    ensureDefault: async (provider) => (await connections.get(`default-${provider}`))!,
  };
  const withCatalog: ProfileDeps = {
    harnessCatalog: fakeHarnessCatalog(),
    connections,
    ...deps,
  };
  const transport = createRouterTransport((router) => registerProfiles(router, withCatalog));
  return {
    client: createClient(ProfileService, transport),
    close: async () => {},
  };
}

async function expectErr(p: Promise<unknown>, code: Code) {
  try { await p; throw new Error(`expected ${Code[code]}`); }
  catch (e) { if (!(e instanceof ConnectError)) throw e; expect(e.code).toBe(code); }
}

const archived: ProfileRow = {
  id: "arch", name: "Archived", description: "", icon: "Bot", imageId: "img-1",
  harness: "claude", model: null, effort: null,
  includeUserTokens: false, envVars: { K: "V" }, skills: [], integrationGrants: [], createdAt: new Date(0), updatedAt: new Date(0),
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] }, secrets: [],
  isDefault: false,
  portExposures: [],
  designation: null,
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
        name: "New", description: "d", icon: "Rocket", imageId: "img-1", harness: "claude", includeUserTokens: true, envVars: { ANTHROPIC_MODEL: "claude-opus-4-8" },
      });
      expect(r.profile!.archived).toBe(false);
      expect(r.profile!.envVars).toEqual({ ANTHROPIC_MODEL: "claude-opus-4-8" });
      expect(r.profile!.includeUserTokens).toBe(true);
    } finally { await s.close(); }
  });

  test("admin CreateProfile sets and returns the PR reviewer designation", async () => {
    const store = makeFakeStore();
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store, images: fakeImages(["img-1"]),
    });
    try {
      const r = await s.client.createProfile({
        name: "Reviewer", description: "", icon: "Bot", imageId: "img-1",
        harness: "claude", includeUserTokens: false, envVars: {},
        designation: "pr_reviewer",
      });
      expect(r.profile!.designation).toBe("pr_reviewer");
      expect((await store.getByDesignation("pr_reviewer"))?.id).toBe(r.profile!.id);
    } finally { await s.close(); }
  });

  test("admin UpdateProfile sets, preserves when omitted, and clears designation", async () => {
    const store = makeFakeStore([active]);
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store, images: fakeImages(["img-1"]),
    });
    const input = {
      id: active.id, name: active.name, description: "", icon: "Bot", imageId: "img-1",
      harness: "claude", includeUserTokens: false, envVars: {},
    };
    try {
      const designated = await s.client.updateProfile({ ...input, designation: "pr_reviewer" });
      expect(designated.profile!.designation).toBe("pr_reviewer");

      const preserved = await s.client.updateProfile({ ...input, name: "Still Reviewer" });
      expect(preserved.profile!.designation).toBe("pr_reviewer");

      const cleared = await s.client.updateProfile({ ...input, designation: "" });
      expect(cleared.profile!.designation).toBeUndefined();
      expect(await store.getByDesignation("pr_reviewer")).toBeNull();
    } finally { await s.close(); }
  });

  test("admin UpdateProfile rejects an unknown designation", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore([active]),
      images: fakeImages(["img-1"]),
    });
    try {
      await expectErr(s.client.updateProfile({
        id: active.id, name: active.name, description: "", icon: "Bot", imageId: "img-1",
        harness: "claude", includeUserTokens: false, envVars: {}, designation: "unknown",
      }), Code.InvalidArgument);
    } finally { await s.close(); }
  });

  test("admin CreateProfile with a bad designation persists no orphan profile", async () => {
    const store = makeFakeStore();
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store, images: fakeImages(["img-1"]),
    });
    try {
      await expectErr(s.client.createProfile({
        name: "Reviewer", description: "", icon: "Bot", imageId: "img-1",
        harness: "claude", includeUserTokens: false, envVars: {}, designation: "unknown",
      }), Code.InvalidArgument);
      // The designation is validated before the row is written, so nothing lands.
      expect(await store.list({ includeArchived: true })).toHaveLength(0);
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
        s.client.createProfile({ name: "x", description: "", icon: "Bot", imageId: "img-1", harness: "claude", includeUserTokens: false, envVars: {}, skills: ["nope"] }),
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
        name: "Skilled", description: "", icon: "Bot", imageId: "img-1", harness: "claude", includeUserTokens: false, envVars: {}, skills: ["skills", "my-linter"],
      });
      expect(r.profile!.skills).toEqual(["skills", "my-linter"]);
    } finally { await s.close(); }
  });

  // ADR 0065: "browser" is a builtin skill (the opt-in in-guest browser). It
  // must validate like any other builtin so a profile can actually select it —
  // the gap that left browserEnabled stuck false even after "adding" it.
  test("admin CreateProfile accepts the builtin browser skill (ADR 0065)", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore(),
      images: fakeImages(["img-1"]), mountCatalog: fakeCatalog([]),
    });
    try {
      const r = await s.client.createProfile({
        name: "Browsable", description: "", icon: "Bot", imageId: "img-1", harness: "claude", includeUserTokens: false, envVars: {}, skills: ["browser"],
      });
      expect(r.profile!.skills).toEqual(["browser"]);
    } finally { await s.close(); }
  });

  test("admin CreateProfile with a malformed capability → InvalidArgument (ADR 0056)", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore(),
      images: fakeImages(["img-1"]), mountCatalog: fakeCatalog([]),
    });
    try {
      await expectErr(
        s.client.createProfile({
          name: "x", description: "", icon: "Bot", imageId: "img-1", harness: "claude",
          includeUserTokens: false, envVars: {},
          integrationGrants: [{ connectionId: "default-github", operation: "", resourceConstraints: [] }],
        }),
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
        name: "Capable", description: "", icon: "Bot", imageId: "img-1", harness: "claude", includeUserTokens: false, envVars: {},
        integrationGrants: [
          protoGrant("github:issues:write"),
          protoGrant("datadog:metrics:read@idx-1"),
        ],
      });
      expect(r.profile!.integrationGrants).toEqual([
        protoGrant("github:issues:write"),
        protoGrant("datadog:metrics:read@idx-1"),
      ]);
    } finally { await s.close(); }
  });

  test("admin CreateProfile and UpdateProfile accept a registered tool capability", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore(),
      images: fakeImages(["img-1"]), mountCatalog: fakeCatalog([]),
      connectors: { list: async () => [] },
      toolCapabilities: new Set([PR_REVIEW_CAPABILITY]),
    });
    try {
      const created = await s.client.createProfile({
        name: "Reviewer", description: "", icon: "ScanSearch", imageId: "img-1",
        harness: "claude", includeUserTokens: false, envVars: {},
        integrationGrants: [protoGrant(PR_REVIEW_CAPABILITY)],
      });
      expect(created.profile!.integrationGrants).toEqual([protoGrant(PR_REVIEW_CAPABILITY)]);

      const updated = await s.client.updateProfile({
        id: created.profile!.id, name: "Reviewer Updated", description: "", icon: "ScanSearch",
        imageId: "img-1", harness: "claude", includeUserTokens: false, envVars: {},
        integrationGrants: [protoGrant(PR_REVIEW_CAPABILITY)],
      });
      expect(updated.profile!.integrationGrants).toEqual([protoGrant(PR_REVIEW_CAPABILITY)]);
    } finally { await s.close(); }
  });

  test("admin CreateProfile still rejects an unregistered non-connector capability", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore(),
      images: fakeImages(["img-1"]), mountCatalog: fakeCatalog([]),
      connectors: { list: async () => [] },
      toolCapabilities: new Set([PR_REVIEW_CAPABILITY]),
    });
    try {
      await expectErr(s.client.createProfile({
        name: "Bogus", description: "", icon: "Bot", imageId: "img-1",
        harness: "claude", includeUserTokens: false, envVars: {},
        integrationGrants: [protoGrant("bogus:thing")],
      }), Code.InvalidArgument);
    } finally { await s.close(); }
  });

  test("Google Cloud grants require a known endpoint-backed operation", async () => {
    const googleConnections: IntegrationConnectionStore = {
      list: async () => [],
      get: async (id) => id === "gcp-1" ? {
        id,
        alias: "prod-readonly",
        provider: "gcp",
        displayName: "Production read only",
        isDefault: false,
        config: {
          workloadIdentityProvider:
            "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/prod",
          serviceAccountEmail: "reader@example-project.iam.gserviceaccount.com",
          endpoints: ["compute.googleapis.com"],
        },
        enabled: true,
        testedAt: new Date(0),
        createdAt: new Date(0),
        updatedAt: new Date(0),
      } : null,
      getDefault: async () => null,
      create: async () => { throw new Error("unused"); },
      update: async () => { throw new Error("unused"); },
      delete: async () => { throw new Error("unused"); },
      markTested: async () => { throw new Error("unused"); },
      setEnabled: async () => { throw new Error("unused"); },
      ensureDefault: async () => { throw new Error("unused"); },
    };
    const s = await spawn({
      getSession: makeGetSession("a", "admin"),
      store: makeFakeStore(),
      images: fakeImages(["img-1"]),
      mountCatalog: fakeCatalog([]),
      connections: googleConnections,
    });
    const request = {
      name: "GCP",
      description: "",
      icon: "Bot",
      imageId: "img-1",
      harness: "claude",
      includeUserTokens: false,
      envVars: {},
      integrationGrants: [{
        connectionId: "gcp-1",
        operation: "compute.instances.get",
        resourceConstraints: [],
      }],
    };
    try {
      await expectErr(s.client.createProfile({
        ...request,
        integrationGrants: [{
          connectionId: "gcp-1",
          operation: "unknown.operation",
          resourceConstraints: [],
        }],
      }), Code.InvalidArgument);
      const created = await s.client.createProfile(request);
      expect(created.profile?.integrationGrants[0]?.operation).toBe("compute.instances.get");
    } finally {
      await s.close();
    }
  });

  // ADR 0064: declarative port exposures round-trip through create/update and
  // default to [] when the field is omitted.
  test("admin CreateProfile round-trips port_exposures + defaults to [] (ADR 0064)", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore(),
      images: fakeImages(["img-1"]), mountCatalog: fakeCatalog([]),
    });
    try {
      const withPorts = await s.client.createProfile({
        name: "Ported", description: "", icon: "Bot", imageId: "img-1", harness: "claude", includeUserTokens: false, envVars: {},
        portExposures: [3000, 8080],
      });
      expect(withPorts.profile!.portExposures).toEqual([3000, 8080]);

      const bare = await s.client.createProfile({
        name: "Bare", description: "", icon: "Bot", imageId: "img-1", harness: "claude", includeUserTokens: false, envVars: {},
      });
      expect(bare.profile!.portExposures).toEqual([]);

      const updated = await s.client.updateProfile({
        id: withPorts.profile!.id, name: "Ported", description: "", icon: "Bot", imageId: "img-1",
        harness: "claude", includeUserTokens: false, envVars: {}, portExposures: [5173],
      });
      expect(updated.profile!.portExposures).toEqual([5173]);
    } finally { await s.close(); }
  });

  test("admin UpdateProfile treats blank optional model and effort as unset", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore([active]),
      images: fakeImages(["img-1"]), mountCatalog: fakeCatalog([]),
    });
    try {
      const updated = await s.client.updateProfile({
        id: active.id, name: active.name, description: "", icon: "Bot", imageId: "img-1",
        harness: "claude", model: "", effort: "", includeUserTokens: false, envVars: {},
      });
      expect(updated.profile!.harness).toBe("claude");
      expect(updated.profile!.model).toBeUndefined();
      expect(updated.profile!.effort).toBeUndefined();
    } finally { await s.close(); }
  });

  test("admin DeleteProfile rejects a designated profile without soft-deleting it", async () => {
    const designated: ProfileRow = { ...active, designation: "pr_reviewer" };
    const baseStore = makeFakeStore([designated]);
    let softDeleteCalls = 0;
    const store: ProfileStore = {
      ...baseStore,
      async softDelete(id) {
        softDeleteCalls += 1;
        await baseStore.softDelete(id);
      },
    };
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store, images: fakeImages(["img-1"]),
    });
    try {
      await expectErr(s.client.deleteProfile({ id: designated.id }), Code.FailedPrecondition);
      expect(softDeleteCalls).toBe(0);
      expect((await store.get(designated.id))?.deletedAt).toBeNull();
    } finally { await s.close(); }
  });
});
