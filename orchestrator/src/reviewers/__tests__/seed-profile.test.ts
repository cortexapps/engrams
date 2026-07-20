import { describe, expect, test } from "bun:test";

import type {
  ProfileInput,
  ProfileRow,
  ProfileStore,
} from "../../db/profiles.ts";
import { PR_REVIEW_CAPABILITY } from "../../tools/review.ts";
import {
  PR_REVIEWER_DESIGNATION,
  seedReviewerProfile,
} from "../seed-profile.ts";

function profileRow(overrides: Partial<ProfileRow> = {}): ProfileRow {
  return {
    id: "default-profile",
    name: "Default",
    description: "",
    icon: "Bot",
    imageId: "default-image",
    harness: "claude",
    model: "sonnet",
    effort: "high",
    includeUserTokens: true,
    envVars: { EXISTING: "value" },
    skills: ["skills"],
    capabilities: ["github:issues:write"],
    network: { default: "allow", allowHosts: ["example.com"], allowHostPatterns: [] },
    secrets: [{ ref: "token", envVar: "TOKEN", mode: "literal", allowHosts: [], allowHostPatterns: [] }],
    isDefault: true,
    portExposures: [3000],
    designation: null,
    createdAt: new Date(0),
    updatedAt: new Date(0),
    deletedAt: null,
    ...overrides,
  };
}

function fakeStore(options: {
  defaultProfile?: ProfileRow | null;
  createError?: unknown;
} = {}): ProfileStore & {
  creates: Array<{ input: ProfileInput; designation: string | null }>;
} {
  const defaultProfile = options.defaultProfile === undefined
    ? profileRow()
    : options.defaultProfile;
  let designatedProfile: ProfileRow | null = null;
  const creates: Array<{ input: ProfileInput; designation: string | null }> = [];

  return {
    creates,
    async list({ includeArchived }) {
      return [defaultProfile, designatedProfile]
        .filter((row): row is ProfileRow => row != null)
        .filter((row) => includeArchived || row.deletedAt == null);
    },
    async get(id) {
      return [defaultProfile, designatedProfile].find((row) => row?.id === id) ?? null;
    },
    async getActive(id) {
      return [defaultProfile, designatedProfile]
        .find((row) => row?.id === id && row.deletedAt == null) ?? null;
    },
    async getDefault() {
      return defaultProfile?.deletedAt == null ? defaultProfile : null;
    },
    async getByDesignation(designation) {
      return designatedProfile?.designation === designation && designatedProfile.deletedAt == null
        ? designatedProfile
        : null;
    },
    async getByIds(ids) {
      return [defaultProfile, designatedProfile]
        .filter((row): row is ProfileRow => row != null && ids.includes(row.id));
    },
    async create(input, designation) {
      if (options.createError) throw options.createError;
      const normalizedDesignation = designation ?? null;
      creates.push({ input, designation: normalizedDesignation });
      designatedProfile = profileRow({
        ...input,
        id: "reviewer-profile",
        designation: normalizedDesignation,
      });
      return designatedProfile;
    },
    async setDesignation(id, designation) {
      if (designatedProfile?.id === id) {
        designatedProfile = { ...designatedProfile, designation };
      }
    },
    async update(id, input) {
      if (designatedProfile?.id !== id || designatedProfile.deletedAt != null) return null;
      designatedProfile = { ...designatedProfile, ...input };
      return designatedProfile;
    },
    async softDelete(id) {
      if (designatedProfile?.id === id) {
        designatedProfile = { ...designatedProfile, deletedAt: new Date(0) };
      }
    },
  };
}

function fakeLogger() {
  const info: string[] = [];
  const debug: Array<{ bindings: Record<string, unknown>; message: string }> = [];
  const error: Array<{ bindings: Record<string, unknown>; message: string }> = [];
  return {
    info,
    debug,
    error,
    logger: {
      info(message: string) { info.push(message); },
      debug(bindings: Record<string, unknown>, message: string) {
        debug.push({ bindings, message });
      },
      error(bindings: Record<string, unknown>, message: string) {
        error.push({ bindings, message });
      },
    },
  };
}

describe("seedReviewerProfile", () => {
  test("creates a designated reviewer by cloning the default's runtime selections", async () => {
    const store = fakeStore();
    const logger = fakeLogger();

    await seedReviewerProfile(store, logger.logger);

    expect(store.creates).toHaveLength(1);
    expect(store.creates[0]).toMatchObject({
      designation: PR_REVIEWER_DESIGNATION,
      input: {
        name: "PR Reviewer",
        imageId: "default-image",
        harness: "claude",
        model: "sonnet",
        effort: "high",
        capabilities: [PR_REVIEW_CAPABILITY],
        includeUserTokens: false,
        envVars: {},
        skills: ["skills"],
        secrets: [],
        portExposures: [],
        isDefault: false,
      },
    });
    expect(store.creates[0]?.input.network).toEqual({
      default: "deny",
      allowHosts: [],
      allowHostPatterns: [],
    });
  });

  test("skips with an informational log when no default exists", async () => {
    const store = fakeStore({ defaultProfile: null });
    const logger = fakeLogger();

    await seedReviewerProfile(store, logger.logger);

    expect(store.creates).toEqual([]);
    expect(logger.info).toEqual([
      "reviewer profile not seeded: configure an org default profile first, then it seeds on next boot (or designate one manually)",
    ]);
  });

  test("is idempotent once the designated profile exists", async () => {
    const store = fakeStore();
    const logger = fakeLogger();

    await seedReviewerProfile(store, logger.logger);
    await seedReviewerProfile(store, logger.logger);

    expect(store.creates).toHaveLength(1);
  });

  test("swallows a concurrent unique violation", async () => {
    const uniqueViolation = Object.assign(new Error("duplicate designation"), { code: "23505" });
    const store = fakeStore({ createError: uniqueViolation });
    const logger = fakeLogger();

    await expect(seedReviewerProfile(store, logger.logger)).resolves.toBeUndefined();
    expect(logger.debug).toHaveLength(1);
    expect(logger.error).toEqual([]);
  });

  test("logs and rethrows a non-unique create failure for the startup caller", async () => {
    const createError = new Error("database unavailable");
    const store = fakeStore({ createError });
    const logger = fakeLogger();

    await expect(seedReviewerProfile(store, logger.logger)).rejects.toBe(createError);
    expect(logger.error).toHaveLength(1);
    expect(logger.debug).toEqual([]);
  });
});
