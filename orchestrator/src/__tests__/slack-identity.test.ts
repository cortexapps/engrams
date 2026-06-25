/**
 * Slack → engrams identity seam (ADR 0060 P2.4, Decision 4).
 *
 * Pure resolution logic with both external reads injected (the Slack client +
 * the email→user lookup), so no live Slack and no DB. The real defaults hit
 * `getSlackClient().users.info` and the better-auth `user` table.
 */

import { expect, test, describe } from "bun:test";
import type { WebClient } from "@slack/web-api";
import { resolveEngramsUser } from "../integrations/slack-identity.ts";

/** A fake WebClient whose users.info returns a fixed profile email. */
const fakeSlack = (email: string | undefined) =>
  ({ users: { info: async () => ({ user: { profile: { email } } }) } }) as unknown as WebClient;

describe("resolveEngramsUser", () => {
  test("matches a slack profile email to an engrams user id", async () => {
    const id = await resolveEngramsUser("slack", "U1", {
      slack: fakeSlack("a@x.com"),
      lookupByEmail: async (e) => (e === "a@x.com" ? "user-1" : null),
    });
    expect(id).toBe("user-1");
  });

  test("no email on the slack profile → null, lookup not attempted", async () => {
    let looked = false;
    const id = await resolveEngramsUser("slack", "U1", {
      slack: fakeSlack(undefined),
      lookupByEmail: async () => {
        looked = true;
        return "x";
      },
    });
    expect(id).toBeNull();
    expect(looked).toBe(false);
  });

  test("email with no matching engrams user → null (unlinked)", async () => {
    const id = await resolveEngramsUser("slack", "U1", {
      slack: fakeSlack("nobody@x.com"),
      lookupByEmail: async () => null,
    });
    expect(id).toBeNull();
  });

  test("a non-slack provider → null (only slack for v1)", async () => {
    const id = await resolveEngramsUser("linear", "U1", {
      slack: fakeSlack("a@x.com"),
      lookupByEmail: async () => "user-1",
    });
    expect(id).toBeNull();
  });
});
