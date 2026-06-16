/**
 * Unit tests for the bootstrap-admin allowlist (restores the pre-ADR-0051
 * `auth.bootstrapAdmins` Helm value). Pure functions — no DB, no server.
 *
 * Covers parsing of ORCHESTRATOR_ADMIN_EMAILS, case/whitespace-insensitive
 * matching, the inert (empty allowlist) posture, and the
 * new-user-vs-existing-user role decisions wired into better-auth's
 * databaseHooks.
 */

import { expect, test, describe } from "bun:test";
import {
  ADMIN_ROLE,
  normalizeEmail,
  parseAdminEmails,
  isBootstrapAdmin,
  promotedRoleOnLogin,
} from "../auth/admin-allowlist.ts";

describe("parseAdminEmails", () => {
  test("unset / empty → []", () => {
    expect(parseAdminEmails(undefined)).toEqual([]);
    expect(parseAdminEmails("")).toEqual([]);
    expect(parseAdminEmails("  ,  ,")).toEqual([]);
  });

  test("splits on commas, trims + lowercases, de-dups", () => {
    expect(parseAdminEmails("Root@Engram.io,  ops@engram.io ")).toEqual([
      "root@engram.io",
      "ops@engram.io",
    ]);
    // duplicate (differing case/whitespace) collapses to one entry
    expect(parseAdminEmails("a@x.io, A@X.IO , a@x.io")).toEqual(["a@x.io"]);
  });
});

describe("normalizeEmail", () => {
  test("trims and lowercases", () => {
    expect(normalizeEmail("  Foo@Bar.COM ")).toBe("foo@bar.com");
  });
});

describe("isBootstrapAdmin", () => {
  const allow = parseAdminEmails("root@engram.io, ops@engram.io");

  test("empty allowlist is always false (inert)", () => {
    expect(isBootstrapAdmin("root@engram.io", [])).toBe(false);
  });

  test("matches case-insensitively + whitespace-tolerantly", () => {
    expect(isBootstrapAdmin("root@engram.io", allow)).toBe(true);
    expect(isBootstrapAdmin("ROOT@Engram.io", allow)).toBe(true);
    expect(isBootstrapAdmin("  ops@engram.io  ", allow)).toBe(true);
  });

  test("non-listed / missing email is false", () => {
    expect(isBootstrapAdmin("nobody@engram.io", allow)).toBe(false);
    expect(isBootstrapAdmin(null, allow)).toBe(false);
    expect(isBootstrapAdmin(undefined, allow)).toBe(false);
    expect(isBootstrapAdmin("", allow)).toBe(false);
  });
});

describe("promotedRoleOnLogin (existing-user sign-in)", () => {
  const allow = parseAdminEmails("root@engram.io");

  test("allowlisted non-admin → promote to admin", () => {
    expect(promotedRoleOnLogin("root@engram.io", "user", allow)).toBe(ADMIN_ROLE);
    // null/undefined role (admin plugin may leave it unset) also promotes
    expect(promotedRoleOnLogin("root@engram.io", null, allow)).toBe(ADMIN_ROLE);
    expect(promotedRoleOnLogin("root@engram.io", undefined, allow)).toBe(ADMIN_ROLE);
  });

  test("already admin → no change", () => {
    expect(promotedRoleOnLogin("root@engram.io", "admin", allow)).toBeUndefined();
  });

  test("not allowlisted → no change regardless of role", () => {
    expect(promotedRoleOnLogin("someone@engram.io", "user", allow)).toBeUndefined();
    expect(promotedRoleOnLogin("someone@engram.io", null, allow)).toBeUndefined();
  });

  test("empty allowlist → never promotes (inert)", () => {
    expect(promotedRoleOnLogin("root@engram.io", "user", [])).toBeUndefined();
  });
});
