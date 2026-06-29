/**
 * Unit tests for the capabilities route (ADR 0064).
 *
 * Mirrors the VNC route's guard harness (src/routes/vnc.test.ts): injects a
 * getSession + resolveOwner so the owner guard passes without real auth, and a
 * resolveSkills stub so the test exercises ONLY the browserEnabled derivation
 * (no DB). The guard reads the user from getSession, so no auth headers are
 * required on the request.
 */
import { describe, it, expect } from "bun:test";
import { makeCapabilitiesRoute, type CapabilitiesDeps } from "./capabilities.ts";

const OWNER = "member-a-cap-test";
const SESSION = "session-owned-by-a-cap";

function makeGetSession(userId: string | null): CapabilitiesDeps["getSession"] {
  return async () =>
    userId
      ? { user: { id: userId, role: "user", email: `${userId}@test` } }
      : null;
}

function makeResolveOwner(): CapabilitiesDeps["resolveOwner"] {
  return async (sid) => (sid === SESSION ? OWNER : null);
}

describe("capabilities route", () => {
  it("returns browserEnabled=true when the profile selects the browser bundle", async () => {
    const app = makeCapabilitiesRoute({
      getSession: makeGetSession(OWNER),
      resolveOwner: makeResolveOwner(),
      resolveSkills: async () => ["skills", "browser"],
    });
    const res = await app.request(`/api/v1/sessions/${SESSION}/capabilities`);
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ browserEnabled: true });
  });

  it("returns browserEnabled=false when browser is not selected", async () => {
    const app = makeCapabilitiesRoute({
      getSession: makeGetSession(OWNER),
      resolveOwner: makeResolveOwner(),
      resolveSkills: async () => ["skills"],
    });
    const res = await app.request(`/api/v1/sessions/${SESSION}/capabilities`);
    expect(res.status).toBe(200);
    expect((await res.json()).browserEnabled).toBe(false);
  });

  it("404s when the caller does not own the session", async () => {
    const app = makeCapabilitiesRoute({
      getSession: makeGetSession("someone-else"),
      resolveOwner: makeResolveOwner(),
      resolveSkills: async () => ["skills", "browser"],
    });
    const res = await app.request(`/api/v1/sessions/${SESSION}/capabilities`);
    expect(res.status).toBe(404);
  });
});
