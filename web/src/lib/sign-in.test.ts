import { describe, expect, it } from "vitest";
import { oauthErrorMessage, oauthReturnUrls } from "./sign-in";

const ORIGIN = "https://engrams.example.com";

describe("oauthReturnUrls", () => {
  it("returns to the app root when there is no next", () => {
    expect(oauthReturnUrls("", ORIGIN)).toEqual({
      callbackURL: `${ORIGIN}/`,
      errorCallbackURL: `${ORIGIN}/login`,
    });
  });

  it("ignores an unrelated query (a previous ?error=)", () => {
    expect(oauthReturnUrls("?error=account_not_allowed", ORIGIN).callbackURL).toBe(`${ORIGIN}/`);
  });

  it("returns to the login page, query intact, so the page can validate next", () => {
    const search = "?next=https%3A%2F%2Fweb-abc123.preview.example.com%2Fa%3Fb%3D1";
    expect(oauthReturnUrls(search, ORIGIN)).toEqual({
      callbackURL: `${ORIGIN}/login${search}`,
      errorCallbackURL: `${ORIGIN}/login`,
    });
  });

  // The server is never handed `next` as a redirect target: whatever it holds,
  // the round trip lands on OUR login page, which validates it (next-url.ts).
  it("a hostile next still returns to this origin's login page", () => {
    const { callbackURL } = oauthReturnUrls("?next=https%3A%2F%2Fevil.example%2F", ORIGIN);
    expect(new URL(callbackURL).origin).toBe(ORIGIN);
    expect(new URL(callbackURL).pathname).toBe("/login");
  });
});

describe("oauthErrorMessage", () => {
  it("is null when there is no error", () => {
    expect(oauthErrorMessage("")).toBeNull();
    expect(oauthErrorMessage("?next=%2Ftasks")).toBeNull();
  });

  it.each([
    ["account_not_allowed", /does not have access/],
    ["email_not_verified", /has not verified/],
    ["email_missing", /did not share an email/],
    ["account_not_linked", /cannot be linked/],
    ["access_denied", /cancelled/],
  ])("explains %s", (code, expected) => {
    expect(oauthErrorMessage(`?error=${code}`)).toMatch(expected);
  });

  it("shows an unknown code, truncated, rather than swallowing it", () => {
    expect(oauthErrorMessage("?error=state_mismatch")).toBe(
      "Sign-in failed (state_mismatch). Try again.",
    );
    const long = "x".repeat(500);
    expect(oauthErrorMessage(`?error=${long}`)!.length).toBeLessThan(120);
  });
});
