import { describe, it, expect } from "vitest";
import { envStem, previewResolved, danglingRefs, type EnvRow } from "./EnvVarsEditor";

const row = (value: string, key = "K"): EnvRow => ({ id: key, key, value });
const apps = [
  { name: "web", port: 5173 },
  { name: "brain-api", port: 8080 },
];

describe("envStem (must match the orchestrator's apps/env.ts)", () => {
  it("uppercases and collapses non-alphanumeric runs to one underscore", () => {
    expect(envStem("web")).toBe("WEB");
    expect(envStem("brain-api")).toBe("BRAIN_API");
    expect(envStem("a--b")).toBe("A_B");
  });
});

describe("previewResolved", () => {
  it("shows the address shape, with the per-session slug as a placeholder", () => {
    expect(previewResolved("${WEB_INGRESS_URL}", apps, "preview.example.com")).toBe(
      "https://web-<session>.preview.example.com",
    );
    expect(previewResolved("${WEB_INGRESS_HOST}", apps, "preview.example.com")).toBe(
      "web-<session>.preview.example.com",
    );
  });

  it("resolves a hyphenated app through its underscored stem", () => {
    expect(previewResolved("${BRAIN_API_INGRESS_URL}", apps, "preview.example.com")).toBe(
      "https://brain-api-<session>.preview.example.com",
    );
  });

  it("keeps surrounding text and resolves several references at once", () => {
    expect(previewResolved("${WEB_INGRESS_URL}/backend", apps, "preview.example.com")).toBe(
      "https://web-<session>.preview.example.com/backend",
    );
    expect(previewResolved("${WEB_INGRESS_HOST},${BRAIN_API_INGRESS_HOST}", apps, "x.io")).toBe(
      "web-<session>.x.io,brain-api-<session>.x.io",
    );
  });

  it("uses http for a local dev domain, matching schemeFor in hostname.ts", () => {
    expect(previewResolved("${WEB_INGRESS_URL}", apps, "lvh.me:8787")).toBe(
      "http://web-<session>.lvh.me:8787",
    );
  });

  it("returns null when there is nothing to resolve", () => {
    expect(previewResolved("plain-value", apps, "preview.example.com")).toBeNull();
    // An undeclared app resolves to nothing — the warning covers that case.
    expect(previewResolved("${API_INGRESS_URL}", apps, "preview.example.com")).toBeNull();
    // No deployment domain → no honest preview to show.
    expect(previewResolved("${WEB_INGRESS_URL}", apps, undefined)).toBeNull();
  });

  it("leaves a reference of any other shape alone", () => {
    // Values legitimately carry shell syntax; only ingress-shaped names are ours.
    expect(previewResolved("${HOME}/bin", apps, "preview.example.com")).toBeNull();
    expect(previewResolved("${WEB_INGRESS_URL}:${PORT}", apps, "x.io")).toBe(
      "https://web-<session>.x.io:${PORT}",
    );
  });
});

describe("danglingRefs", () => {
  it("reports an ingress-shaped reference that names no app", () => {
    expect(danglingRefs([row("${API_INGRESS_URL}")], apps)).toEqual(["${API_INGRESS_URL}"]);
  });

  it("stays quiet for declared apps and for non-ingress references", () => {
    expect(danglingRefs([row("${WEB_INGRESS_URL}"), row("${HOME}", "H")], apps)).toEqual([]);
  });

  it("deduplicates across rows", () => {
    expect(
      danglingRefs([row("${API_INGRESS_URL}", "A"), row("${API_INGRESS_URL}", "B")], apps),
    ).toEqual(["${API_INGRESS_URL}"]);
  });

  it("treats every app as undeclared when there are no apps", () => {
    expect(danglingRefs([row("${WEB_INGRESS_URL}")], [])).toEqual(["${WEB_INGRESS_URL}"]);
  });
});
