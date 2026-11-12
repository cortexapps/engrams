import { describe, expect, test } from "bun:test";

import { resolveEngramsUser } from "../identity.ts";

const chat = (email: string | null) => (provider: string) =>
  provider === "chat-a" ? { userEmail: async () => email } : null;

describe("resolveEngramsUser", () => {
  test("matches the chat user's email address to an engrams user", async () => {
    const id = await resolveEngramsUser("chat-a", "U1", {
      chat: chat("a@example.com"),
      lookupByEmail: async (email) => (email === "a@example.com" ? "user-1" : null),
    });
    expect(id).toBe("user-1");
  });

  test("no email address is no user, and the lookup does not run", async () => {
    let looked = false;
    const id = await resolveEngramsUser("chat-a", "U1", {
      chat: chat(null),
      lookupByEmail: async () => {
        looked = true;
        return "user-1";
      },
    });
    expect(id).toBeNull();
    expect(looked).toBe(false);
  });

  test("an email address with no engrams user is no user", async () => {
    const id = await resolveEngramsUser("chat-a", "U1", {
      chat: chat("nobody@example.com"),
      lookupByEmail: async () => null,
    });
    expect(id).toBeNull();
  });

  test("a provider that is not a chat provider is no user", async () => {
    const id = await resolveEngramsUser("other", "U1", {
      chat: chat("a@example.com"),
      lookupByEmail: async () => "user-1",
    });
    expect(id).toBeNull();
  });
});
