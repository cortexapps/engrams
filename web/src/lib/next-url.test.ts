/**
 * `?next=` validation (ADR 0118).
 *
 * This is an open-redirect guard, so the tests are written as an attack list:
 * every case that must NOT navigate away, plus the two that must.
 */

import { describe, test, expect } from "vitest";
import { safeNextUrl, DEFAULT_AFTER_LOGIN } from "./next-url";

const ORIGIN = "https://app.example.com";
const PREVIEW = "preview.example.com";

const next = (raw: string, base: string | undefined = PREVIEW) =>
  safeNextUrl(`?next=${encodeURIComponent(raw)}`, base, ORIGIN);

describe("safeNextUrl — allowed destinations", () => {
  test("a path on this origin", () => {
    expect(next("/tasks/42")).toBe(`${ORIGIN}/tasks/42`);
  });

  test("an absolute URL on this origin", () => {
    expect(next(`${ORIGIN}/settings`)).toBe(`${ORIGIN}/settings`);
  });

  test("a single label under the preview base domain", () => {
    // The case the whole parameter exists for: return the user to the app they
    // were trying to open.
    expect(next("https://web-tidy-swift-otters.preview.example.com/page")).toBe(
      "https://web-tidy-swift-otters.preview.example.com/page",
    );
  });
});

describe("safeNextUrl — refused destinations", () => {
  test("a foreign origin", () => {
    expect(next("https://evil.example.com/steal")).toBe(DEFAULT_AFTER_LOGIN);
  });

  test("a protocol-relative URL, which is a foreign origin in disguise", () => {
    // "//evil.com" looks like a path but resolves to a different host.
    expect(next("//evil.com/steal")).toBe(DEFAULT_AFTER_LOGIN);
  });

  test("a javascript: or data: URL", () => {
    // `new URL` parses both happily; navigating to them executes.
    expect(next("javascript:alert(1)")).toBe(DEFAULT_AFTER_LOGIN);
    expect(next("data:text/html,<script>alert(1)</script>")).toBe(DEFAULT_AFTER_LOGIN);
  });

  test("a suffix near-miss on the preview domain", () => {
    // `evilpreview.example.com` ends with neither ".preview.example.com" nor
    // the origin, and must not be mistaken for a preview host.
    expect(next("https://evilpreview.example.com/")).toBe(DEFAULT_AFTER_LOGIN);
    expect(next("https://preview.example.com.evil.test/")).toBe(DEFAULT_AFTER_LOGIN);
  });

  test("a NESTED label under the preview domain", () => {
    // The preview handler routes exactly one label; anything deeper is not a
    // host it serves, so it is not a host we bounce to.
    expect(next("https://a.b.preview.example.com/")).toBe(DEFAULT_AFTER_LOGIN);
  });

  test("the preview apex itself", () => {
    expect(next("https://preview.example.com/")).toBe(DEFAULT_AFTER_LOGIN);
  });

  test("a preview URL when the deployment has no preview domain", () => {
    const raw = `?next=${encodeURIComponent("https://web-x.preview.example.com/")}`;
    expect(safeNextUrl(raw, "", ORIGIN)).toBe(DEFAULT_AFTER_LOGIN);
    expect(safeNextUrl(raw, undefined, ORIGIN)).toBe(DEFAULT_AFTER_LOGIN);
  });

  test("an absent or empty next", () => {
    expect(safeNextUrl("", PREVIEW, ORIGIN)).toBe(DEFAULT_AFTER_LOGIN);
    expect(safeNextUrl("?next=", PREVIEW, ORIGIN)).toBe(DEFAULT_AFTER_LOGIN);
  });
});
