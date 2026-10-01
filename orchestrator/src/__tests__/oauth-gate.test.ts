/**
 * auth/oauth-gate.ts — who may come in through the `oauth` door. Pure unit
 * tests of the decision function over provider claims; the end-to-end flow
 * (real better-auth callback against a fake provider) is oauth-login.test.ts.
 */

import { expect, test, describe } from "bun:test";
import {
  allowlistIsEmpty,
  checkOAuthProfile,
  displayNameFromClaims,
  isGoogleIssuer,
  parseEmailAllowlist,
  profileFromClaims,
  type EmailAllowlist,
} from "../auth/oauth-gate.ts";

const GOOGLE = "https://accounts.google.com";
const OKTA = "https://corp.okta.com";

const CORP: EmailAllowlist = { domains: ["corp.com"], emails: [] };

describe("parseEmailAllowlist", () => {
  test("unset → empty", () => {
    const a = parseEmailAllowlist(undefined, undefined, []);
    expect(a).toEqual({ domains: [], emails: [] });
    expect(allowlistIsEmpty(a)).toBe(true);
  });

  test("normalises and folds in the bootstrap admins", () => {
    const a = parseEmailAllowlist(" @Corp.com,corp.com , other.io", "A@X.dev", ["root@corp.com"]);
    expect(a).toEqual({ domains: ["corp.com", "other.io"], emails: ["a@x.dev", "root@corp.com"] });
    expect(allowlistIsEmpty(a)).toBe(false);
  });
});

describe("isGoogleIssuer", () => {
  test("matches Google with or without the trailing slash, nothing else", () => {
    expect(isGoogleIssuer(GOOGLE)).toBe(true);
    expect(isGoogleIssuer(`${GOOGLE}/`)).toBe(true);
    expect(isGoogleIssuer("https://accounts.google.com.evil.example")).toBe(false);
    expect(isGoogleIssuer(OKTA)).toBe(false);
  });
});

describe("checkOAuthProfile — the verified-email requirement", () => {
  test("no email → refused", () => {
    expect(checkOAuthProfile({ sub: "1" }, OKTA, CORP)).toBe("email_missing");
    expect(checkOAuthProfile({ email: "not-an-email" }, OKTA, CORP)).toBe("email_missing");
  });

  test.each([undefined, false, "false", 1, "yes"])(
    "email_verified=%p → refused, even for an allowed domain",
    (v) => {
      expect(checkOAuthProfile({ email: "a@corp.com", email_verified: v }, OKTA, CORP)).toBe(
        "email_not_verified",
      );
    },
  );

  test("an unverified email is refused BEFORE the allowlist, so an explicit entry cannot rescue it", () => {
    const allow: EmailAllowlist = { domains: ["*"], emails: ["a@corp.com"] };
    expect(checkOAuthProfile({ email: "a@corp.com" }, OKTA, allow)).toBe("email_not_verified");
  });

  test('the string "true" counts as verified', () => {
    expect(checkOAuthProfile({ email: "a@corp.com", email_verified: "true" }, OKTA, CORP)).toBeNull();
  });
});

describe("checkOAuthProfile — the allowlist (generic provider)", () => {
  const verified = (email: string) => ({ email, email_verified: true });

  test("an allowed domain is admitted, case-insensitively", () => {
    expect(checkOAuthProfile(verified("Alice@Corp.com"), OKTA, CORP)).toBeNull();
  });

  test("another domain is refused", () => {
    expect(checkOAuthProfile(verified("alice@other.com"), OKTA, CORP)).toBe("account_not_allowed");
  });

  test("a lookalike domain is refused: suffix and subdomain do not match", () => {
    expect(checkOAuthProfile(verified("a@evilcorp.com"), OKTA, CORP)).toBe("account_not_allowed");
    expect(checkOAuthProfile(verified("a@sub.corp.com"), OKTA, CORP)).toBe("account_not_allowed");
    expect(checkOAuthProfile(verified("a@corp.com.evil.io"), OKTA, CORP)).toBe(
      "account_not_allowed",
    );
  });

  test("an address with two @ is judged by its real domain", () => {
    expect(checkOAuthProfile(verified("a@corp.com@evil.io"), OKTA, CORP)).toBe(
      "account_not_allowed",
    );
  });

  test("an explicit email is admitted whatever its domain", () => {
    const allow: EmailAllowlist = { domains: ["corp.com"], emails: ["contractor@other.dev"] };
    expect(checkOAuthProfile(verified("Contractor@other.dev"), OKTA, allow)).toBeNull();
    expect(checkOAuthProfile(verified("someone@other.dev"), OKTA, allow)).toBe(
      "account_not_allowed",
    );
  });

  test('"*" admits every verified account', () => {
    const any: EmailAllowlist = { domains: ["*"], emails: [] };
    expect(checkOAuthProfile(verified("anyone@anywhere.io"), OKTA, any)).toBeNull();
  });
});

describe("checkOAuthProfile — Google vouches by `hd`, not by the address", () => {
  test("a Workspace account of the allowed domain is admitted", () => {
    expect(
      checkOAuthProfile({ email: "a@corp.com", email_verified: true, hd: "corp.com" }, GOOGLE, CORP),
    ).toBeNull();
  });

  // The case the `hd` rule exists for: a personal Google account registered
  // with a corp address. Google marks it verified, and it survives the person
  // leaving the company — the Workspace admin cannot deprovision it.
  test("a consumer account with an @corp.com address (no hd) is refused", () => {
    expect(checkOAuthProfile({ email: "a@corp.com", email_verified: true }, GOOGLE, CORP)).toBe(
      "account_not_allowed",
    );
  });

  test("a Workspace account of ANOTHER organization is refused", () => {
    expect(
      checkOAuthProfile(
        { email: "a@other.com", email_verified: true, hd: "other.com" },
        GOOGLE,
        CORP,
      ),
    ).toBe("account_not_allowed");
  });

  test("hd decides, not the address: a corp address under another hd is refused", () => {
    expect(
      checkOAuthProfile({ email: "a@corp.com", email_verified: true, hd: "other.com" }, GOOGLE, CORP),
    ).toBe("account_not_allowed");
  });

  test("an allowed secondary-domain address under the primary hd is admitted", () => {
    expect(
      checkOAuthProfile(
        { email: "a@corp.co.uk", email_verified: true, hd: "corp.com" },
        GOOGLE,
        CORP,
      ),
    ).toBeNull();
  });

  test("Gmail is a domain Google vouches for without hd", () => {
    const gmail: EmailAllowlist = { domains: ["gmail.com"], emails: [] };
    expect(checkOAuthProfile({ email: "a@gmail.com", email_verified: true }, GOOGLE, gmail)).toBeNull();
    expect(checkOAuthProfile({ email: "a@gmail.com", email_verified: true }, GOOGLE, CORP)).toBe(
      "account_not_allowed",
    );
  });

  test("an explicit email admits a consumer account — a named, per-person grant", () => {
    const allow: EmailAllowlist = { domains: [], emails: ["a@corp.com"] };
    expect(checkOAuthProfile({ email: "a@corp.com", email_verified: true }, GOOGLE, allow)).toBeNull();
  });

  test('"*" admits a consumer account with any address', () => {
    const any: EmailAllowlist = { domains: ["*"], emails: [] };
    expect(checkOAuthProfile({ email: "a@corp.com", email_verified: true }, GOOGLE, any)).toBeNull();
  });
});

describe("displayNameFromClaims", () => {
  test("the provider's full name is used as it is", () => {
    expect(displayNameFromClaims({ name: " Nikhil Unni ", email: "nikhil@corp.com" })).toBe(
      "Nikhil Unni",
    );
  });

  // Some directories fill `name` with the address or a username. The two
  // structured claims are then the better source.
  test.each([
    ["an email address", "nikhil@corp.com"],
    ["one word", "nunni"],
    ["nothing", undefined],
  ])("when name is %s, given + family name are used", (_label, name) => {
    expect(
      displayNameFromClaims({ name, given_name: "Nikhil", family_name: "Unni", email: "n@corp.com" }),
    ).toBe("Nikhil Unni");
  });

  test("a one-word name stands when there is no given + family pair", () => {
    expect(displayNameFromClaims({ name: "Cher", email: "cher@corp.com" })).toBe("Cher");
    expect(displayNameFromClaims({ name: "Cher", given_name: "Cher", email: "c@corp.com" })).toBe(
      "Cher",
    );
  });

  test("with no name at all, the mailbox name stands in", () => {
    expect(displayNameFromClaims({ email: "Jdoe@Corp.com" })).toBe("jdoe");
  });
});

describe("profileFromClaims — a whitelist", () => {
  test("only id, email, emailVerified, name and image leave", () => {
    expect(
      profileFromClaims({
        iss: "https://idp.example",
        aud: "cid",
        sub: "idp-1",
        email: "Alice@Corp.com",
        email_verified: true,
        name: "Alice A",
        picture: "https://idp.example/a.png",
        hd: "corp.com",
        // Claims a provider can be configured to send. None may reach the row.
        role: "admin",
        banned: true,
        banReason: "x",
        groups: ["admins"],
        id: "attacker-chosen-row-id",
        createdAt: "1970-01-01",
      }),
    ).toEqual({
      id: "idp-1",
      email: "alice@corp.com",
      emailVerified: true,
      name: "Alice A",
      image: "https://idp.example/a.png",
    });
  });

  test("no picture → no image key", () => {
    expect(
      profileFromClaims({ sub: "1", email: "a@corp.com", email_verified: true, name: "A B" }),
    ).toEqual({ id: "1", email: "a@corp.com", emailVerified: true, name: "A B" });
  });

  test("a numeric subject is kept, as a string", () => {
    expect(profileFromClaims({ sub: 42, email: "a@corp.com", name: "A B" })?.id).toBe("42");
  });

  test("no subject → no profile", () => {
    expect(profileFromClaims({ email: "a@corp.com", name: "A B" })).toBeNull();
    expect(profileFromClaims({ sub: "  ", email: "a@corp.com", name: "A B" })).toBeNull();
  });
});
