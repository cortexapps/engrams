/**
 * Task creation (ADR 0060 P2.7) — the shared create path.
 *
 * `compileSessionCreateInput` turns a profile into a CreateSession request;
 * `createTaskWithSession` is the ONE create path (UI CreateTask RPC + the
 * external-trigger ThreadControlPlane both call it): compile → create session →
 * persist task + primary task_session, compensating the orphan session on a DB
 * failure. Pure given injected deps — unit-tested with fakes for images /
 * connectors / the user-token resolver / the upstream session client / the DB.
 */

import { expect, test, describe } from "bun:test";
import { z } from "zod";
import {
  compileSessionCreateInput,
  createSessionForExistingTask,
  createTaskWithSession,
  truncatePrompt,
  type SessionCompileDeps,
  type CreateTaskDeps,
  type TaskSessionsClient,
  type HarnessCatalogClient,
  type Db,
} from "../rpc/task-create.ts";
import type { ProfileRow, ProfileStore } from "../db/profiles.ts";
import type {
  PortExposureStore,
  PortExposureInput,
  PortExposureRow,
} from "../db/port-exposures.ts";
import type { ImagesClient } from "../rpc/profiles.ts";
import type { UserIdentity, UserIdentityStore } from "../db/users.ts";
import { createToolRegistry } from "../tools/registry.ts";
import { BASE_SYSTEM_PROMPT } from "../prompts/base.ts";
import { OAuthSubjectKind } from "../gen/engram/app/v1/oauth_pb.ts";

// The claude harness declares this as its `auth.user_env` (see fakeHarnessCatalog);
// the compiler injects the user token under this name (ADR 0063 — descriptor-driven).
const USER_ENV = "CLAUDE_CODE_OAUTH_TOKEN";
// The claude harness's declared `auth.org_env` (programmatic credential, B4).
const ORG_ENV = "ANTHROPIC_API_KEY";

const profile = (over: Partial<ProfileRow> = {}): ProfileRow => ({
  id: "p1",
  name: "P",
  description: "",
  icon: "Bot",
  imageId: "img-1",
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
  designation: null,
  createdAt: new Date(0),
  updatedAt: new Date(0),
  deletedAt: null,
  ...over,
});

// A catalog with one harness ("claude"): opus default + sonnet model, high effort.
// The compiler maps the resolved model/effort id → these env vars (ADR 0063 §1).
const fakeHarnessCatalog = (): HarnessCatalogClient => ({
  listHarnesses: async () => ({
    harnesses: [
      {
        name: "claude",
        descriptor: {
          auth: { userEnv: USER_ENV, orgEnv: ORG_ENV },
          models: [
            { id: "opus", default: true, env: { ANTHROPIC_MODEL: "claude-opus-4-8" } },
            { id: "sonnet", default: false, env: { ANTHROPIC_MODEL: "claude-sonnet-4-6" } },
          ],
          effort: [{ id: "high", default: true, env: { MAX_THINKING_TOKENS: "32000" } }],
        },
      },
    ],
  }),
});

// Default the user token to present ("tok") — a human (chat) run now BLOCKS when
// the harness's declared user_env is unset, so tests exercising other
// seams must have a token unless they specifically test the block.
const deps = (
  token: string | null = "tok",
  images = [{ id: "img-1", imageUri: "uri-1" }],
  allTokens: Record<string, string> = {},
): SessionCompileDeps => ({
  images: { listEnabledImages: async () => ({ images }) } as unknown as ImagesClient,
  connectors: { list: async () => [] },
  harnessCatalog: fakeHarnessCatalog(),
  resolveUserToken: async () => token,
  resolveAllUserTokens: async () => allTokens,
});

describe("truncatePrompt — default title from the prompt", () => {
  test("short prompt is used verbatim", () => {
    expect(truncatePrompt("Fix the bug")).toBe("Fix the bug");
  });

  test("collapses whitespace/newlines and trims", () => {
    expect(truncatePrompt("  Fix   the\n\tbug  ")).toBe("Fix the bug");
  });

  test("clips a long prompt to ~80 code points with an ellipsis", () => {
    const long = "a".repeat(200);
    const out = truncatePrompt(long)!;
    expect(out.endsWith("…")).toBe(true);
    expect([...out].length).toBe(81); // 80 chars + ellipsis
  });

  test("does not split a multi-byte glyph at the boundary", () => {
    // 81 emoji: clipping at 80 code points must not produce a lone surrogate.
    const emoji = "😀".repeat(81);
    const out = truncatePrompt(emoji)!;
    expect(out.endsWith("…")).toBe(true);
    // Every code point before the ellipsis is a whole emoji.
    expect([...out.slice(0, -1)].every((c) => c === "😀")).toBe(true);
  });

  test("empty / whitespace-only / null → null (no default)", () => {
    expect(truncatePrompt("")).toBe(null);
    expect(truncatePrompt("   \n  ")).toBe(null);
    expect(truncatePrompt(undefined)).toBe(null);
    expect(truncatePrompt(null)).toBe(null);
  });
});

describe("compileSessionCreateInput", () => {
  test("resolves the profile image → imageUri, mode agent", async () => {
    const inp = await compileSessionCreateInput(profile(), deps());
    expect(inp.imageUri).toBe("uri-1");
    expect(inp.mode).toBe("agent");
  });

  test("sets the base system prompt with no extra harness env", async () => {
    const inp = await compileSessionCreateInput(profile(), deps());
    expect(inp.harnessEnv?.ENGRAM_APPEND_SYSTEM_PROMPT).toBe(BASE_SYSTEM_PROMPT);
  });

  test("enables private browser image observations only with the browser skill", async () => {
    const withBrowser = await compileSessionCreateInput(profile({ skills: ["browser"] }), deps());
    expect(withBrowser.selectedSkills).toEqual(["browser"]);
    expect(withBrowser.harnessEnv?.ENGRAM_BROWSER_VIEW_ENABLED).toBe("1");

    const withoutBrowser = await compileSessionCreateInput(profile(), deps(), {
      extraHarnessEnv: { ENGRAM_BROWSER_VIEW_ENABLED: "1" },
    });
    expect(withoutBrowser.harnessEnv?.ENGRAM_BROWSER_VIEW_ENABLED).toBeUndefined();
  });

  test("appends the base prompt after extraHarnessEnv's system prompt", async () => {
    const inp = await compileSessionCreateInput(profile({ envVars: { FOO: "bar" } }), deps(), {
      extraHarnessEnv: { ENGRAM_APPEND_SYSTEM_PROMPT: "be concise" },
    });
    expect(inp.harnessEnv).toMatchObject({
      FOO: "bar",
      ENGRAM_APPEND_SYSTEM_PROMPT: `be concise\n\n${BASE_SYSTEM_PROMPT}`,
    });
  });

  // The harness's declared user credential is MANDATORY for a human
  // run — always injected, independent of includeUserTokens.
  test("always injects the harness user_env for a human run, regardless of includeUserTokens", async () => {
    const off = await compileSessionCreateInput(profile({ includeUserTokens: false }), deps("tok"));
    expect(off.harnessEnv?.[USER_ENV]).toBe("tok");
    const on = await compileSessionCreateInput(profile({ includeUserTokens: true }), deps("tok"));
    expect(on.harnessEnv?.[USER_ENV]).toBe("tok");
  });

  test("human auth is principal-authoritative over profile and trigger env values", async () => {
    const inp = await compileSessionCreateInput(
      profile({ envVars: { [USER_ENV]: "profile-token", [ORG_ENV]: "profile-org-token" } }),
      deps("user-token"),
      { extraHarnessEnv: { [USER_ENV]: "trigger-token", [ORG_ENV]: "trigger-org-token" } },
    );
    expect(inp.harnessEnv?.[USER_ENV]).toBe("user-token");
    expect(inp.harnessEnv?.[ORG_ENV]).toBeUndefined();
  });

  // Don't silently boot un-authed — block the create with the env-var
  // name in the message (the client renders the descriptor's setup hint).
  test("blocks a human run when the harness user credential is not set", async () => {
    await expect(compileSessionCreateInput(profile(), deps(null))).rejects.toThrow(
      /CLAUDE_CODE_OAUTH_TOKEN.*isn't set/s,
    );
  });

  // includeUserTokens now means "also carry my OTHER saved tokens".
  test("includeUserTokens additionally injects the user's OTHER saved tokens", async () => {
    const inp = await compileSessionCreateInput(
      profile({ includeUserTokens: true }),
      deps("tok", undefined, { GH_TOKEN: "ghp", [USER_ENV]: "tok" }),
    );
    expect(inp.harnessEnv?.GH_TOKEN).toBe("ghp");
    expect(inp.harnessEnv?.[USER_ENV]).toBe("tok");
  });

  test("without includeUserTokens, other saved tokens are NOT injected (only the harness user_env)", async () => {
    const inp = await compileSessionCreateInput(
      profile({ includeUserTokens: false }),
      deps("tok", undefined, { GH_TOKEN: "ghp" }),
    );
    expect(inp.harnessEnv?.GH_TOKEN).toBeUndefined();
    expect(inp.harnessEnv?.[USER_ENV]).toBe("tok");
  });

  test("injects the user token under the harness's declared user_env, not a hardcoded name", async () => {
    const customDeps: SessionCompileDeps = {
      images: {
        listEnabledImages: async () => ({ images: [{ id: "img-1", imageUri: "uri-1" }] }),
      } as unknown as ImagesClient,
      connectors: { list: async () => [] },
      harnessCatalog: {
        listHarnesses: async () => ({
          harnesses: [
            { name: "claude", descriptor: { auth: { userEnv: "OPENCODE_TOKEN" }, models: [], effort: [] } },
          ],
        }),
      },
      resolveUserToken: async (envVar) => (envVar === "OPENCODE_TOKEN" ? "tok-123" : null),
      resolveAllUserTokens: async () => ({}),
    };
    const inp = await compileSessionCreateInput(profile({ includeUserTokens: true }), customDeps);
    expect(inp.harnessEnv?.OPENCODE_TOKEN).toBe("tok-123");
    expect(inp.harnessEnv?.CLAUDE_CODE_OAUTH_TOKEN).toBeUndefined();
  });

  // ADR 0063 B4 (amended): strict-by-principal credentials.
  test("a human-owned task injects user_env and no org_env secret", async () => {
    // The PRINCIPAL decides the credential, never the task type/surface: a
    // Slack mention email-matched to a real engrams user rides that user's
    // token exactly like the chat UI. (Under the old `type === "chat"` gate,
    // Slack session e721311e booted credential-less — "Not logged in".)
    const inp = await compileSessionCreateInput(profile({ includeUserTokens: true }), deps("tok"), {});
    expect(inp.harnessEnv?.[USER_ENV]).toBe("tok");
    const policy = inp.integrationPolicyJson
      ? (JSON.parse(inp.integrationPolicyJson) as { secrets?: Array<{ env_var: string }> })
      : { secrets: [] };
    expect((policy.secrets ?? []).some((s) => s.env_var === ORG_ENV)).toBe(false);
  });

  test("a task from a SERVICE-ACCOUNT principal rides org_env (the CI smoke regression)", async () => {
    // A `ci-<repo>` API key has no per-user harness token — the programmatic
    // flag must pick the org-credential path or the harness boots
    // credential-less ("not logged in", session 47723225).
    const inp = await compileSessionCreateInput(
      profile({
        includeUserTokens: true,
        envVars: { [USER_ENV]: "profile-user-token", [ORG_ENV]: "profile-org-token" },
      }),
      deps("tok"),
      {
        programmatic: true,
        extraHarnessEnv: { [USER_ENV]: "trigger-user-token", [ORG_ENV]: "trigger-org-token" },
      },
    );
    expect(inp.harnessEnv?.[USER_ENV]).toBeUndefined();
    expect(inp.harnessEnv?.[ORG_ENV]).toBeUndefined();
    const policy = JSON.parse(inp.integrationPolicyJson!) as {
      secrets?: Array<{ secret_ref: string; env_var: string; mode: string }>;
    };
    expect((policy.secrets ?? []).find((s) => s.env_var === ORG_ENV)).toMatchObject({
      secret_ref: ORG_ENV,
      env_var: ORG_ENV,
      mode: "literal",
    });
  });

  test("binds a human Codex OAuth connection without putting OAuth bytes in harnessEnv", async () => {
    const codexDeps: SessionCompileDeps = {
      ...deps(null),
      harnessCatalog: {
        listHarnesses: async () => ({
          harnesses: [{
            name: "codex",
            descriptor: {
              label: "Codex",
              auth: {
                userOauth: { provider: "openai-codex", delivery: 1 },
                orgEnv: "CODEX_API_KEY",
              },
              models: [],
              effort: [],
            },
          }],
        }),
      },
      hasOAuthCredential: async (provider) => provider === "openai-codex",
      oauthSubject: {
        kind: OAuthSubjectKind.OAUTH_SUBJECT_KIND_USER,
        id: "user-1",
      },
    };
    const input = await compileSessionCreateInput(profile({ harness: "codex" }), codexDeps);
    expect(input.oauthCredential).toEqual({
      subject: { kind: OAuthSubjectKind.OAUTH_SUBJECT_KIND_USER, id: "user-1" },
      provider: "openai-codex",
    });
    expect(input.harnessEnv?.OPENAI_API_KEY).toBeUndefined();
    expect(JSON.stringify(input.harnessEnv)).not.toContain("openai-codex");
  });

  test("blocks disconnected human Codex but keeps the service-account API-key path", async () => {
    const codexDeps: SessionCompileDeps = {
      ...deps(null),
      harnessCatalog: {
        listHarnesses: async () => ({
          harnesses: [{
            name: "codex",
            descriptor: {
              label: "Codex",
              auth: {
                userOauth: { provider: "openai-codex", delivery: 1 },
                orgEnv: "CODEX_API_KEY",
              },
              models: [],
              effort: [],
            },
          }],
        }),
      },
      hasOAuthCredential: async () => false,
      oauthSubject: {
        kind: OAuthSubjectKind.OAUTH_SUBJECT_KIND_USER,
        id: "user-1",
      },
    };
    await expect(
      compileSessionCreateInput(profile({ harness: "codex" }), codexDeps),
    ).rejects.toThrow(/Settings → Credentials/);

    const serviceInput = await compileSessionCreateInput(
      profile({ harness: "codex" }),
      codexDeps,
      { programmatic: true },
    );
    expect(serviceInput.oauthCredential).toBeUndefined();
    const policy = JSON.parse(serviceInput.integrationPolicyJson!) as {
      secrets?: Array<{ env_var: string }>;
    };
    expect(policy.secrets?.some((secret) => secret.env_var === "CODEX_API_KEY")).toBe(true);
  });

  test("passes the prompt through when set", async () => {
    const inp = await compileSessionCreateInput(profile(), deps(), { prompt: "do the thing" });
    expect(inp.prompt).toBe("do the thing");
  });

  test("throws if the profile image is no longer enabled", async () => {
    await expect(compileSessionCreateInput(profile(), deps(null, []))).rejects.toThrow(/no longer enabled/);
  });

  // ADR 0063 B2: harness / model / effort resolution + env mapping.
  test("defaults to the deployment harness + descriptor default model/effort env", async () => {
    const inp = await compileSessionCreateInput(profile(), deps());
    expect(inp.harness).toBe("claude");
    expect(inp.harnessEnv).toMatchObject({
      ANTHROPIC_MODEL: "claude-opus-4-8",
      MAX_THINKING_TOKENS: "32000",
    });
  });

  test("profile default harness/model resolve when no override", async () => {
    const inp = await compileSessionCreateInput(profile({ harness: "claude", model: "sonnet" }), deps());
    expect(inp.harness).toBe("claude");
    expect(inp.harnessEnv?.ANTHROPIC_MODEL).toBe("claude-sonnet-4-6");
  });

  test("per-session model override beats the profile default", async () => {
    const inp = await compileSessionCreateInput(profile({ model: "opus" }), deps(), { model: "sonnet" });
    expect(inp.harnessEnv?.ANTHROPIC_MODEL).toBe("claude-sonnet-4-6");
  });

  test("an explicit model picker wins over a stale ANTHROPIC_MODEL in env_vars", async () => {
    const inp = await compileSessionCreateInput(
      profile({ envVars: { ANTHROPIC_MODEL: "stale" } }),
      deps(),
      { model: "sonnet" },
    );
    expect(inp.harnessEnv?.ANTHROPIC_MODEL).toBe("claude-sonnet-4-6");
  });

  // ADR 0031 §7: git commit attribution.
  test("owner identity stamps ENGRAM_USER_NAME/EMAIL for the guest gitconfig", async () => {
    const inp = await compileSessionCreateInput(profile(), deps(), {
      owner: { name: "Ada Lovelace", email: "ada@example.com" },
    });
    expect(inp.harnessEnv).toMatchObject({
      ENGRAM_USER_NAME: "Ada Lovelace",
      ENGRAM_USER_EMAIL: "ada@example.com",
    });
  });

  test("no owner → no attribution env", async () => {
    const inp = await compileSessionCreateInput(profile(), deps());
    expect(inp.harnessEnv?.ENGRAM_USER_EMAIL).toBeUndefined();
    expect(inp.harnessEnv?.ENGRAM_USER_NAME).toBeUndefined();
  });

  test("injects ENGRAM_TOOLS with capability-gated and ungated manifest tools", async () => {
    const toolRegistry = createToolRegistry();
    toolRegistry.register({
      name: "always_available",
      description: "Available to every profile.",
      input: z.object({ value: z.string() }),
      output: z.object({ ok: z.boolean() }),
      handling: "handled",
      execution: "sync",
      handler: async () => ({ ok: true }),
    });
    toolRegistry.register({
      name: "save_memory",
      description: "Save a note.",
      input: z.object({ text: z.string() }),
      output: z.object({ saved: z.boolean() }),
      handling: "handled",
      execution: "sync",
      capability: "memory:write",
      handler: async () => ({ saved: true }),
    });
    toolRegistry.register({
      name: "admin_only",
      description: "Requires another capability.",
      input: z.object({}),
      output: z.object({ ok: z.boolean() }),
      handling: "handled",
      execution: "sync",
      capability: "admin:tools",
      handler: async () => ({ ok: true }),
    });

    const inp = await compileSessionCreateInput(
      profile({ capabilities: ["memory:write"] }),
      { ...deps(), toolRegistry },
    );
    const manifest = JSON.parse(inp.harnessEnv!.ENGRAM_TOOLS!) as Array<{ name: string }>;
    expect(manifest.map((tool) => tool.name)).toEqual(["always_available", "save_memory"]);
  });

  test("omits ENGRAM_TOOLS when no registered tool matches the profile", async () => {
    const toolRegistry = createToolRegistry();
    toolRegistry.register({
      name: "save_memory",
      description: "Save a note.",
      input: z.object({ text: z.string() }),
      output: z.object({ saved: z.boolean() }),
      handling: "handled",
      execution: "sync",
      capability: "memory:write",
      handler: async () => ({ saved: true }),
    });

    const inp = await compileSessionCreateInput(profile(), { ...deps(), toolRegistry });
    expect(inp.harnessEnv?.ENGRAM_TOOLS).toBeUndefined();
  });

  test("extra capabilities widen integration grants but never the tool manifest", async () => {
    const cloneCapability = "github:contents:read@openai/engrams";
    const toolRegistry = createToolRegistry();
    toolRegistry.register({
      name: "review_tool",
      description: "Profile-granted review tool.",
      input: z.object({}),
      output: z.object({ ok: z.boolean() }),
      handling: "handled",
      execution: "sync",
      capability: "engram:pr_review",
      handler: async () => ({ ok: true }),
    });
    toolRegistry.register({
      name: "must_not_leak",
      description: "A tool gated only by the per-session integration grant.",
      input: z.object({}),
      output: z.object({ ok: z.boolean() }),
      handling: "handled",
      execution: "sync",
      capability: cloneCapability,
      handler: async () => ({ ok: true }),
    });

    const inp = await compileSessionCreateInput(
      profile({ capabilities: ["engram:pr_review"] }),
      { ...deps(), toolRegistry },
      { extraCapabilities: [cloneCapability, cloneCapability] },
    );

    expect(inp.capabilities).toEqual(["engram:pr_review", cloneCapability]);
    const policy = JSON.parse(inp.integrationPolicyJson!) as {
      injects?: Array<{ mint_provider: string }>;
    };
    expect(policy.injects?.some((entry) => entry.mint_provider === "github")).toBe(true);
    const manifest = JSON.parse(inp.harnessEnv!.ENGRAM_TOOLS!) as Array<{ name: string }>;
    expect(manifest.map((tool) => tool.name)).toEqual(["review_tool"]);
  });

  test("session clamps replace capabilities/network and drop profile secrets/env", async () => {
    const reviewCapability = "engram:pr_review";
    const cloneCapability = "github:contents:read@openai/engrams";
    const toolRegistry = createToolRegistry();
    toolRegistry.register({
      name: "review_tool",
      description: "Allowed by the review clamp.",
      input: z.object({}),
      output: z.object({ ok: z.boolean() }),
      handling: "handled",
      execution: "sync",
      capability: reviewCapability,
      handler: async () => ({ ok: true }),
    });
    toolRegistry.register({
      name: "profile_write_tool",
      description: "Must not survive the review clamp.",
      input: z.object({}),
      output: z.object({ ok: z.boolean() }),
      handling: "handled",
      execution: "sync",
      capability: "github:pulls:write",
      handler: async () => ({ ok: true }),
    });

    const inp = await compileSessionCreateInput(
      profile({
        capabilities: ["github:pulls:write"],
        network: {
          default: "allow",
          allowHosts: ["profile.example.com"],
          allowHostPatterns: ["*.profile.example.com"],
        },
        secrets: [{
          ref: "PROFILE_PAT",
          envVar: "PROFILE_PAT",
          mode: "literal",
          allowHosts: ["github.com"],
          allowHostPatterns: [],
        }],
        envVars: {
          PROFILE_ONLY: "must-drop",
          ANTHROPIC_MODEL: "stale-profile-model",
        },
      }),
      { ...deps(), toolRegistry },
      {
        capabilityOverride: [reviewCapability, cloneCapability],
        extraCapabilities: ["github:issues:write"],
        networkOverride: {
          default: "deny",
          allowHosts: ["github.com", "codeload.github.com", "api.github.com"],
          allowHostPatterns: [],
        },
        dropProfileSecretsAndEnv: true,
        extraHarnessEnv: { TRIGGER_ONLY: "kept" },
      },
    );

    expect(inp.capabilities).toEqual([reviewCapability, cloneCapability]);
    const policy = JSON.parse(inp.integrationPolicyJson!) as {
      network: {
        default: string;
        allow_hosts: string[];
        allow_host_patterns: string[];
      };
      secrets: unknown[];
    };
    expect(policy.network).toEqual({
      default: "deny",
      allow_hosts: ["github.com", "codeload.github.com", "api.github.com"],
      allow_host_patterns: [],
    });
    expect(policy.secrets).toEqual([]);
    expect(inp.harnessEnv?.PROFILE_ONLY).toBeUndefined();
    expect(inp.harnessEnv).toMatchObject({
      ANTHROPIC_MODEL: "claude-opus-4-8",
      TRIGGER_ONLY: "kept",
    });
    const manifest = JSON.parse(inp.harnessEnv!.ENGRAM_TOOLS!) as Array<{ name: string }>;
    expect(manifest.map((tool) => tool.name)).toEqual(["review_tool"]);
  });
});

// ---------------------------------------------------------------------------
// createTaskWithSession — the shared create path
// ---------------------------------------------------------------------------

const fakeProfiles = (active = true, over: Partial<ProfileRow> = {}): ProfileStore =>
  ({ getActive: async (id: string) => (active && id === "p1" ? profile(over) : null) }) as unknown as ProfileStore;

/** A full PortExposureStore fake that records createOrGet inputs and can be made
 *  to throw for a given port (to exercise the best-effort path). */
function fakePortExposures(opts: { failOnPort?: number } = {}): PortExposureStore & {
  calls: PortExposureInput[];
} {
  const calls: PortExposureInput[] = [];
  return {
    calls,
    async createOrGet(input: PortExposureInput): Promise<PortExposureRow> {
      calls.push(input);
      if (opts.failOnPort === input.port) throw new Error("port store boom");
      return {
        slug: `slug-${input.port}`,
        sessionId: input.sessionId,
        port: input.port,
        label: input.label,
        ownerUserId: input.ownerUserId,
        visibility: input.visibility,
        shareToken: null,
        createdAt: new Date(0),
        expiresAt: null,
      };
    },
    async listBySession() {
      return [];
    },
    async getBySlug() {
      return null;
    },
    async deleteBySlug() {
      return false;
    },
  };
}

function fakeSessions(): TaskSessionsClient & { createReqs: unknown[]; deletedIds: string[] } {
  const createReqs: unknown[] = [];
  const deletedIds: string[] = [];
  return {
    createReqs,
    deletedIds,
    createSession: async (req) => {
      createReqs.push(req);
      return { sessionId: "sess-1" };
    },
    deleteSession: async (req) => {
      deletedIds.push(req.sessionId);
      return {};
    },
  };
}

/** A fake DB that records each `.values()` payload in insert order, or throws
 * from the transaction / a selected insert to exercise compensation paths. */
function recordingDb(
  records: Record<string, unknown>[],
  throwOnTx = false,
  failInsertAt?: number,
): Db {
  return {
    transaction: async (fn: (tx: unknown) => Promise<unknown>) => {
      if (throwOnTx) throw new Error("db boom");
      let insertCount = 0;
      const tx = {
        insert: () => ({
          values: async (v: Record<string, unknown>) => {
            insertCount += 1;
            if (insertCount === failInsertAt) throw new Error("insert boom");
            records.push(v);
          },
        }),
      };
      return fn(tx);
    },
  } as unknown as Db;
}

function fakeUsers(
  getIdentity: UserIdentityStore["getIdentity"] = async () => null,
): UserIdentityStore {
  return {
    getIdentity,
    async getIdentities(userIds) {
      const entries = await Promise.all(
        userIds.map(async (id) => [id, await getIdentity(id)] as const),
      );
      return new Map(
        entries.filter((entry): entry is readonly [string, UserIdentity] => entry[1] != null),
      );
    },
  };
}

const createDeps = (
  sessions: TaskSessionsClient,
  db: Db,
  opts: {
    active?: boolean;
    profileOver?: Partial<ProfileRow>;
    portExposures?: PortExposureStore;
    users?: CreateTaskDeps["users"];
  } = {},
): CreateTaskDeps => ({
  profiles: fakeProfiles(opts.active ?? true, opts.profileOver ?? {}),
  images: { listEnabledImages: async () => ({ images: [{ id: "img-1", imageUri: "uri-1" }] }) } as unknown as ImagesClient,
  connectors: { list: async () => [] },
  harnessCatalog: fakeHarnessCatalog(),
  sessions,
  // Token present by default so human (chat) creates don't hit the block.
  secrets: { get: async () => "tok", getAll: async () => ({}) },
  db,
  // Default to "unknown user" so tests exercising other seams don't hit the
  // real Drizzle fallback against the fake Db.
  users: opts.users ?? fakeUsers(),
  ...(opts.portExposures ? { portExposures: opts.portExposures } : {}),
});

describe("createTaskWithSession", () => {
  test("writes the per-session listener row in the task transaction", async () => {
    const records: Record<string, unknown>[] = [];
    await createTaskWithSession(
      createDeps(fakeSessions(), recordingDb(records)),
      { type: "chat", ownerUserId: "user-1", profileId: "p1" },
    );

    expect(records[2]).toEqual({ sessionId: "sess-1" });
  });

  test("persists task + primary task_session and folds extraHarnessEnv into the session", async () => {
    const records: Record<string, unknown>[] = [];
    const sessions = fakeSessions();

    const out = await createTaskWithSession(createDeps(sessions, recordingDb(records)), {
      type: "slack_thread",
      ownerUserId: "user-1",
      profileId: "p1",
      source: { provider: "slack", team: "T1" },
      extraHarnessEnv: { ENGRAM_APPEND_SYSTEM_PROMPT: "be concise" },
      slackThreadWorkflowId: "thread-wf-1",
    });

    expect(out.sessionId).toBe("sess-1");
    expect(typeof out.taskId).toBe("string");
    expect((sessions.createReqs[0] as { harnessEnv?: Record<string, string> }).harnessEnv?.ENGRAM_APPEND_SYSTEM_PROMPT).toBe(
      `be concise\n\n${BASE_SYSTEM_PROMPT}`,
    );
    // records[0] = task, records[1] = primary task_session.
    expect(records[0]).toMatchObject({
      type: "slack_thread",
      createdByUserId: "user-1",
      status: "open",
      source: { provider: "slack", team: "T1" },
    });
    expect(records[1]).toMatchObject({ sessionId: "sess-1", role: "primary", profileId: "p1" });
    expect(records[2]).toEqual({ sessionId: "sess-1", threadWfId: "thread-wf-1" });
    expect(records[3]).toEqual({ sessionId: "sess-1" });
  });

  test("defaults source to {} and title to null", async () => {
    const records: Record<string, unknown>[] = [];
    await createTaskWithSession(createDeps(fakeSessions(), recordingDb(records)), {
      type: "chat",
      ownerUserId: "u",
      profileId: "p1",
    });
    expect(records[0]!.source).toEqual({});
    expect(records[0]!.title).toBeNull();
  });

  test("throws NotFound for a missing/archived profile and creates no session", async () => {
    const sessions = fakeSessions();
    await expect(
      createTaskWithSession(createDeps(sessions, recordingDb([]), { active: false }), {
        type: "chat",
        ownerUserId: "u",
        profileId: "p1",
      }),
    ).rejects.toThrow(/not found or archived/);
    expect(sessions.createReqs).toHaveLength(0);
  });

  // A human chat task with no user credential is blocked BEFORE any
  // session is created (the block is in the compile step).
  test("blocks a human chat task when the harness user credential is unset, creating no session", async () => {
    const sessions = fakeSessions();
    const deps: CreateTaskDeps = {
      ...createDeps(sessions, recordingDb([])),
      secrets: { get: async () => null, getAll: async () => ({}) },
    };
    await expect(
      createTaskWithSession(deps, { type: "chat", ownerUserId: "u", profileId: "p1" }),
    ).rejects.toThrow(/CLAUDE_CODE_OAUTH_TOKEN/);
    expect(sessions.createReqs).toHaveLength(0);
  });

  test("compensates by deleting the orphan session when the DB write fails", async () => {
    const sessions = fakeSessions();
    await expect(
      createTaskWithSession(createDeps(sessions, recordingDb([], true)), {
        type: "chat",
        ownerUserId: "u",
        profileId: "p1",
      }),
    ).rejects.toThrow(/db boom/);
    expect(sessions.deletedIds).toEqual(["sess-1"]);
  });

  test("compensates when listener registration fails inside the task transaction", async () => {
    const sessions = fakeSessions();
    await expect(
      createTaskWithSession(createDeps(sessions, recordingDb([], false, 3)), {
        type: "chat",
        ownerUserId: "u",
        profileId: "p1",
      }),
    ).rejects.toThrow(/insert boom/);
    expect(sessions.deletedIds).toEqual(["sess-1"]);
  });

  // ADR 0064: a profile's declared portExposures auto-mint one private exposure
  // per port at session create, against the injected PortExposureStore.
  test("auto-mints one private port-exposure per profile.portExposures port", async () => {
    const records: Record<string, unknown>[] = [];
    const ports = fakePortExposures();
    const out = await createTaskWithSession(
      createDeps(fakeSessions(), recordingDb(records), {
        profileOver: { portExposures: [3000, 8080] },
        portExposures: ports,
      }),
      { type: "chat", ownerUserId: "user-1", profileId: "p1" },
    );

    expect(ports.calls).toHaveLength(2);
    expect(ports.calls[0]).toEqual({
      sessionId: out.sessionId,
      port: 3000,
      label: "",
      ownerUserId: "user-1",
      visibility: "private",
    });
    expect(ports.calls[1]).toMatchObject({ port: 8080, visibility: "private", ownerUserId: "user-1" });
  });

  test("does NOT mint when the profile declares no portExposures", async () => {
    const ports = fakePortExposures();
    await createTaskWithSession(
      createDeps(fakeSessions(), recordingDb([]), { portExposures: ports }),
      { type: "chat", ownerUserId: "u", profileId: "p1" },
    );
    expect(ports.calls).toHaveLength(0);
  });

  // ADR 0031 §7: the owner's identity threads into the session's harness env
  // for git commit attribution — except for service accounts, whose synthetic
  // email is not a valid commit author.
  test("threads the owner's identity into the session create for git attribution", async () => {
    const sessions = fakeSessions();
    await createTaskWithSession(
      createDeps(sessions, recordingDb([]), {
        users: fakeUsers(async (id) =>
          id === "user-1" ? { name: "Ada", email: "ada@example.com" } : null
        ),
      }),
      { type: "chat", ownerUserId: "user-1", profileId: "p1" },
    );
    expect((sessions.createReqs[0] as { harnessEnv?: Record<string, string> }).harnessEnv).toMatchObject({
      ENGRAM_USER_NAME: "Ada",
      ENGRAM_USER_EMAIL: "ada@example.com",
    });
  });

  test("a service-account owner (API-key create) gets no git attribution", async () => {
    const sessions = fakeSessions();
    await createTaskWithSession(
      createDeps(sessions, recordingDb([]), {
        users: fakeUsers(async () => ({ name: "svc", email: "apikey+abc@service.local" })),
      }),
      { type: "chat", ownerUserId: "svc-1", profileId: "p1" },
    );
    const env = (sessions.createReqs[0] as { harnessEnv?: Record<string, string> }).harnessEnv;
    expect(env?.ENGRAM_USER_EMAIL).toBeUndefined();
    expect(env?.ENGRAM_USER_NAME).toBeUndefined();
  });

  test("an identity-lookup failure is best-effort: the task still creates, unattributed", async () => {
    const sessions = fakeSessions();
    const out = await createTaskWithSession(
      createDeps(sessions, recordingDb([]), {
        users: fakeUsers(async () => {
          throw new Error("identity boom");
        }),
      }),
      { type: "chat", ownerUserId: "user-1", profileId: "p1" },
    );
    expect(out.sessionId).toBe("sess-1");
    const env = (sessions.createReqs[0] as { harnessEnv?: Record<string, string> }).harnessEnv;
    expect(env?.ENGRAM_USER_EMAIL).toBeUndefined();
  });

  test("a port-exposure failure does NOT fail the task (best-effort) and later ports still mint", async () => {
    const records: Record<string, unknown>[] = [];
    const ports = fakePortExposures({ failOnPort: 3000 });
    const out = await createTaskWithSession(
      createDeps(fakeSessions(), recordingDb(records), {
        profileOver: { portExposures: [3000, 8080] },
        portExposures: ports,
      }),
      { type: "chat", ownerUserId: "u", profileId: "p1" },
    );

    // Task still created + persisted despite the 3000 failure.
    expect(out.sessionId).toBe("sess-1");
    expect(records).toHaveLength(3); // task + primary task_session + listener
    // Both ports were attempted; 8080 succeeded after 3000 threw.
    expect(ports.calls.map((c) => c.port)).toEqual([3000, 8080]);
  });
});

describe("createSessionForExistingTask", () => {
  test("creates promptlessly and leaves listener registration off by default", async () => {
    const records: Record<string, unknown>[] = [];
    const sessions = fakeSessions();

    const out = await createSessionForExistingTask(
      createDeps(sessions, recordingDb(records)),
      {
        taskId: "task-existing",
        profileId: "p1",
        role: "finder",
        extraCapabilities: ["github:contents:read@openai/engrams"],
        appendSystemPrompt: "finder system prompt",
      },
    );

    expect(out).toEqual({ sessionId: "sess-1" });
    // The effective granted set (profile caps + extras) is persisted on the
    // task_session so the tool-exec gate honors it — the profile itself has no
    // capabilities here, yet the session carries the extra grant.
    expect(records).toEqual([{
      taskId: "task-existing",
      sessionId: "sess-1",
      role: "finder",
      profileId: "p1",
      capabilities: ["github:contents:read@openai/engrams"],
    }]);
    const request = sessions.createReqs[0] as {
      prompt?: string;
      capabilities?: string[];
      harnessEnv?: Record<string, string>;
    };
    expect(request.prompt).toBeUndefined();
    expect(request.capabilities).toEqual(["github:contents:read@openai/engrams"]);
    expect(request.harnessEnv?.ENGRAM_APPEND_SYSTEM_PROMPT).toBe(
      `finder system prompt\n\n${BASE_SYSTEM_PROMPT}`,
    );
  });

  test("registers the listener in the same transaction when requested", async () => {
    const records: Record<string, unknown>[] = [];

    await createSessionForExistingTask(
      createDeps(fakeSessions(), recordingDb(records)),
      {
        taskId: "task-existing",
        profileId: "p1",
        role: "verifier",
        registerListener: true,
      },
    );

    expect(records).toEqual([
      {
        taskId: "task-existing",
        sessionId: "sess-1",
        role: "verifier",
        profileId: "p1",
        capabilities: [],
      },
      { sessionId: "sess-1" },
    ]);
  });

  test("threads policy clamps into the existing-task session compiler", async () => {
    const sessions = fakeSessions();
    await createSessionForExistingTask(
      createDeps(sessions, recordingDb([]), {
        profileOver: {
          capabilities: ["github:pulls:write"],
          envVars: { PROFILE_PAT: "must-drop" },
          network: {
            default: "allow",
            allowHosts: ["profile.example.com"],
            allowHostPatterns: [],
          },
          secrets: [{
            ref: "PROFILE_PAT",
            envVar: "PROFILE_PAT",
            mode: "literal",
            allowHosts: ["github.com"],
            allowHostPatterns: [],
          }],
        },
      }),
      {
        taskId: "task-existing",
        profileId: "p1",
        role: "finder",
        capabilityOverride: [
          "engram:pr_review",
          "github:contents:read@openai/engrams",
        ],
        networkOverride: {
          default: "deny",
          allowHosts: ["github.com", "codeload.github.com", "api.github.com"],
          allowHostPatterns: [],
        },
        dropProfileSecretsAndEnv: true,
      },
    );

    const request = sessions.createReqs[0] as {
      capabilities?: string[];
      harnessEnv?: Record<string, string>;
      integrationPolicyJson?: string;
    };
    expect(request.capabilities).toEqual([
      "engram:pr_review",
      "github:contents:read@openai/engrams",
    ]);
    expect(request.harnessEnv?.PROFILE_PAT).toBeUndefined();
    const policy = JSON.parse(request.integrationPolicyJson!) as {
      network: { allow_hosts: string[] };
      secrets: Array<{ secret_ref: string }>;
    };
    expect(policy.network.allow_hosts).toEqual([
      "github.com",
      "codeload.github.com",
      "api.github.com",
    ]);
    expect(policy.secrets.some((secret) => secret.secret_ref === "PROFILE_PAT")).toBe(false);
  });

  test("compensates when requested listener registration fails", async () => {
    const sessions = fakeSessions();
    await expect(createSessionForExistingTask(
      createDeps(sessions, recordingDb([], false, 2)),
      {
        taskId: "task-existing",
        profileId: "p1",
        role: "verifier",
        registerListener: true,
      },
    )).rejects.toThrow(/insert boom/);
    expect(sessions.deletedIds).toEqual(["sess-1"]);
  });

  test("deletes the orphan session when task_session persistence fails", async () => {
    const sessions = fakeSessions();
    await expect(createSessionForExistingTask(
      createDeps(sessions, recordingDb([], true)),
      { taskId: "task-existing", profileId: "p1", role: "finder" },
    )).rejects.toThrow(/db boom/);
    expect(sessions.deletedIds).toEqual(["sess-1"]);
  });
});
