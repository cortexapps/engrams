import { describe, expect, test } from "bun:test";

import { abilityFor } from "../ability.ts";
import { canAccessSession, type IsReviewWorkerSession } from "../session-access.ts";

const MEMBER = { id: "member-1", role: "user" };
const OTHER_MEMBER_ID = "member-2";
const ADMIN = { id: "admin-1", role: "admin" };

const REVIEWER_SESSION = "session-review-finder";

/** Records whether the lookup was consulted, so the tests can assert that an
 *  ordinary denial never pays for a DB round-trip. */
function reviewWorkerSpy(
  isWorker: (sessionId: string) => boolean = (sid) => sid === REVIEWER_SESSION,
): { fn: IsReviewWorkerSession; calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    fn: async (sessionId) => {
      calls.push(sessionId);
      return isWorker(sessionId);
    },
  };
}

describe("canAccessSession", () => {
  test("an owned session is readable without consulting the review lookup", async () => {
    const spy = reviewWorkerSpy();

    expect(
      await canAccessSession(abilityFor(MEMBER), "read", "s1", MEMBER.id, spy.fn),
    ).toBe(true);
    expect(spy.calls).toEqual([]);
  });

  // ADR 0100 d10. A pr_review task carries createdByUserId: null, so the owner
  // check can never match and this derived read is the only thing that lets the
  // review page link to the transcript behind a finding.
  test("a review's worker session is readable by any member", async () => {
    const spy = reviewWorkerSpy();

    expect(
      await canAccessSession(
        abilityFor(MEMBER),
        "read",
        REVIEWER_SESSION,
        null,
        spy.fn,
      ),
    ).toBe(true);
    expect(spy.calls).toEqual([REVIEWER_SESSION]);
  });

  test.each(["prompt", "shell", "delete"] as const)(
    "a review's worker session is not %s-able — read only",
    async (action) => {
      const spy = reviewWorkerSpy();

      expect(
        await canAccessSession(
          abilityFor(MEMBER),
          action,
          REVIEWER_SESSION,
          null,
          spy.fn,
        ),
      ).toBe(false);
      // Short-circuited before the lookup: the action disqualifies it outright.
      expect(spy.calls).toEqual([]);
    },
  );

  test("an unowned session that no review names stays denied", async () => {
    const spy = reviewWorkerSpy(() => false);

    expect(
      await canAccessSession(abilityFor(MEMBER), "read", "s-unknown", null, spy.fn),
    ).toBe(false);
    expect(spy.calls).toEqual(["s-unknown"]);
  });

  // The narrowing that keeps this from costing a query per denied probe: a
  // reviewer session is always unowned, so a session with a real owner cannot
  // be one and the lookup is skipped.
  test("a session owned by someone else never reaches the lookup", async () => {
    const spy = reviewWorkerSpy();

    expect(
      await canAccessSession(
        abilityFor(MEMBER),
        "read",
        REVIEWER_SESSION,
        OTHER_MEMBER_ID,
        spy.fn,
      ),
    ).toBe(false);
    expect(spy.calls).toEqual([]);
  });

  test("admins are allowed by the ownership check alone", async () => {
    const spy = reviewWorkerSpy();

    expect(
      await canAccessSession(abilityFor(ADMIN), "delete", REVIEWER_SESSION, null, spy.fn),
    ).toBe(true);
    expect(spy.calls).toEqual([]);
  });
});
