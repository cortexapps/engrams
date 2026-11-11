import { describe, expect, test } from "bun:test";
import type { WebClient } from "@slack/web-api";

// This module loads before the chat registry here; the registry must not
// read a provider's spec while it loads.
import { makeSlackChat, SLACK_CHAT } from "../chat.ts";
import { chatFor, threadsBuiltinKey } from "../../../chat/registry.ts";
import { declaredIngress } from "../../../connectors/registry.ts";

/** A client whose users.info returns a fixed profile email. */
const client = (email: string | undefined) => async () =>
  ({ users: { info: async () => ({ user: { profile: { email } } }) } }) as unknown as WebClient;

describe("Slack as a chat provider", () => {
  test("the registry serves it", () => {
    expect(chatFor(SLACK_CHAT.provider)?.provider).toBe("slack");
    expect(threadsBuiltinKey("slack")).toBe("slack_brain");
    expect(threadsBuiltinKey("linear")).toBeNull();
  });

  test("userEmail is the profile email, or null when the profile has none", async () => {
    expect(await makeSlackChat(client("a@x.com")).userEmail("U1")).toBe("a@x.com");
    expect(await makeSlackChat(client(undefined)).userEmail("U1")).toBeNull();
    expect(await makeSlackChat(client("")).userEmail("U1")).toBeNull();
  });

  test("the connector declares the interactivity path the provider serves", () => {
    expect(declaredIngress("slack")?.ingress.interactivity).toBe(true);
    expect(SLACK_CHAT.interactivity).toBeDefined();
  });

  test("a thread handle names the channel and the thread root", () => {
    expect(makeSlackChat(client(undefined)).threadHandle({ channel: "C1", threadRoot: "100.0" })).toBe(
      "slack:C1:100.0",
    );
  });
});
