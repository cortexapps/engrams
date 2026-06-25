/**
 * Profile → CreateSession compilation (ADR 0059 P2.7).
 *
 * The shared seam CreateTask and the external-trigger ThreadControlPlane both
 * use, so a triggered session runs with the SAME capabilities/network/secrets/
 * skills as a UI task (no new privilege path). Pure given its injected deps —
 * unit-tested with fakes for images / connectors / the user-token resolver.
 */

import { expect, test, describe } from "bun:test";
import { compileSessionCreateInput, type SessionCompileDeps } from "../rpc/session-compile.ts";
import { CLAUDE_OAUTH_ENV_VAR } from "../db/user-secrets.ts";
import type { ProfileRow } from "../db/profiles.ts";
import type { ImagesClient } from "../rpc/profiles.ts";

const profile = (over: Partial<ProfileRow> = {}): ProfileRow => ({
  id: "p1",
  name: "P",
  description: "",
  icon: "Bot",
  imageId: "img-1",
  includeUserTokens: false,
  envVars: {},
  skills: [],
  capabilities: [],
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
  secrets: [],
  isDefault: false,
  createdAt: new Date(0),
  updatedAt: new Date(0),
  deletedAt: null,
  ...over,
});

const deps = (token: string | null = null, images = [{ id: "img-1", imageUri: "uri-1" }]): SessionCompileDeps => ({
  images: { listEnabledImages: async () => ({ images }) } as unknown as ImagesClient,
  connectors: { list: async () => [] },
  resolveUserToken: async () => token,
});

describe("compileSessionCreateInput", () => {
  test("resolves the profile image → imageUri, mode agent", async () => {
    const inp = await compileSessionCreateInput(profile(), deps());
    expect(inp.imageUri).toBe("uri-1");
    expect(inp.mode).toBe("agent");
  });

  test("merges extraHarnessEnv last (e.g. ENGRAM_APPEND_SYSTEM_PROMPT)", async () => {
    const inp = await compileSessionCreateInput(profile({ envVars: { FOO: "bar" } }), deps(), {
      extraHarnessEnv: { ENGRAM_APPEND_SYSTEM_PROMPT: "be concise" },
    });
    expect(inp.harnessEnv).toMatchObject({ FOO: "bar", ENGRAM_APPEND_SYSTEM_PROMPT: "be concise" });
  });

  test("user token injected only when includeUserTokens", async () => {
    const off = await compileSessionCreateInput(profile({ includeUserTokens: false }), deps("tok"));
    expect(off.harnessEnv?.[CLAUDE_OAUTH_ENV_VAR]).toBeUndefined();
    const on = await compileSessionCreateInput(profile({ includeUserTokens: true }), deps("tok"));
    expect(on.harnessEnv?.[CLAUDE_OAUTH_ENV_VAR]).toBe("tok");
  });

  test("passes the prompt through when set", async () => {
    const inp = await compileSessionCreateInput(profile(), deps(), { prompt: "do the thing" });
    expect(inp.prompt).toBe("do the thing");
  });

  test("throws if the profile image is no longer enabled", async () => {
    await expect(compileSessionCreateInput(profile(), deps(null, []))).rejects.toThrow(/no longer enabled/);
  });
});
