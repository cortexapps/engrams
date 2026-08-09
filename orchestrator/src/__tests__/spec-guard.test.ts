import { describe, expect, test } from "bun:test";

import {
  authorizeSpecMemberAccess,
  makeSpecMemberHeaderGuard,
  type GetSession,
  type ResolveSpecMembership,
} from "../routes/guard.ts";

const authenticated: GetSession = async () => ({
  user: { id: "member-2", role: "user" },
});

describe("the organization-shared spec guard", () => {
  test("admits a member who does not own the spec", async () => {
    const membershipChecks: Array<[string, string]> = [];
    const resolveMembership: ResolveSpecMembership = async (specId, userId) => {
      membershipChecks.push([specId, userId]);
      return true;
    };

    const result = await authorizeSpecMemberAccess(
      new Headers(),
      "spec-owned-by-member-1",
      authenticated,
      resolveMembership,
    );

    expect(result).toEqual({ ok: true, user: { id: "member-2", role: "user" } });
    expect(membershipChecks).toEqual([["spec-owned-by-member-1", "member-2"]]);
  });

  test("rejects an unauthenticated caller before the membership lookup", async () => {
    let checkedMembership = false;
    const resolveMembership: ResolveSpecMembership = async () => {
      checkedMembership = true;
      return true;
    };

    const result = await authorizeSpecMemberAccess(
      new Headers(),
      "spec-1",
      async () => null,
      resolveMembership,
    );

    expect(result).toEqual({ ok: false, status: 401 });
    expect(checkedMembership).toBe(false);
  });

  test("uses Not Found for an unknown spec or a non-member", async () => {
    const guard = makeSpecMemberHeaderGuard(async () => false, authenticated);

    await expect(guard(new Headers(), "spec-outside-org")).resolves.toEqual({
      ok: false,
      status: 404,
    });
    await expect(guard(new Headers(), undefined)).resolves.toEqual({
      ok: false,
      status: 404,
    });
  });
});
