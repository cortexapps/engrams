/**
 * SessionAppsSection (ADR 0118) — the Diagnostics-drawer app list:
 * external-link rows + a liveness dot (gated on the session being active) +
 * publish/validate, against a mocked REST API.
 */

import { describe, test, expect, vi, afterEach } from "vitest";
import { screen, fireEvent, waitFor } from "@testing-library/react";
import { renderWithProviders } from "../../test-utils";
import { SessionAppsSection } from "./SessionAppsSection";

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

const app = {
  hostLabel: "web-jumping-fat-kittens",
  sessionId: "s1",
  name: "web",
  port: 3000,
  visibility: "org" as const,
  url: "https://web-jumping-fat-kittens.preview.example.com",
  createdAt: "",
};

afterEach(() => vi.unstubAllGlobals());

describe("SessionAppsSection", () => {
  test("shows the empty state", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => json({ apps: [] })),
    );
    renderWithProviders(<SessionAppsSection sessionId="s1" active={false} />);
    await screen.findByTestId("ports-empty");
  });

  test("renders an app as an external link", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) =>
        url.endsWith("/health") ? json({ status: "up" }) : json({ apps: [app] }),
      ),
    );
    renderWithProviders(<SessionAppsSection sessionId="s1" active={true} />);
    const link = await screen.findByTestId("open-web-jumping-fat-kittens");
    expect(link.getAttribute("href")).toBe("https://web-jumping-fat-kittens.preview.example.com");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.textContent).toContain(":3000");
    expect(link.textContent).toContain("web");
  });

  test("inactive session: NO liveness probe, dot stays unknown", async () => {
    const fetchMock = vi.fn(async (url: string) =>
      url.endsWith("/health") ? json({ status: "up" }) : json({ apps: [app] }),
    );
    vi.stubGlobal("fetch", fetchMock);

    renderWithProviders(<SessionAppsSection sessionId="s1" active={false} />);
    const dot = await screen.findByTestId("liveness-web-jumping-fat-kittens");
    expect(dot.getAttribute("data-health")).toBe("unknown");
    expect(fetchMock.mock.calls.some((c) => String(c[0]).endsWith("/health"))).toBe(false);
  });

  test("active session: probes and reflects 'up'", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) =>
        url.endsWith("/health") ? json({ status: "up" }) : json({ apps: [app] }),
      ),
    );
    renderWithProviders(<SessionAppsSection sessionId="s1" active={true} />);
    await waitFor(() => {
      const dot = screen.getByTestId("liveness-web-jumping-fat-kittens");
      expect(dot.getAttribute("data-health")).toBe("up");
    });
  });

  test("a duplicate port (409) gets a friendly message", async () => {
    const fetchMock = vi.fn(async (_url: string, init?: RequestInit) => {
      if ((init?.method ?? "GET") === "POST") return json({ error: "exists" }, 409);
      return json({ apps: [] });
    });
    vi.stubGlobal("fetch", fetchMock);

    renderWithProviders(<SessionAppsSection sessionId="s1" active={false} />);
    await screen.findByTestId("ports-empty");
    fireEvent.change(screen.getByTestId("port-input"), { target: { value: "3000" } });
    fireEvent.click(screen.getByTestId("expose-btn"));

    const err = await screen.findByTestId("ports-error");
    expect(err.textContent).toContain("already published");
  });

  test("rejects an out-of-range port without calling the API", async () => {
    const fetchMock = vi.fn(async (_url: string, _init?: RequestInit) => json({ apps: [] }));
    vi.stubGlobal("fetch", fetchMock);

    renderWithProviders(<SessionAppsSection sessionId="s1" active={false} />);
    await screen.findByTestId("ports-empty");
    fireEvent.change(screen.getByTestId("port-input"), { target: { value: "0" } });
    fireEvent.click(screen.getByTestId("expose-btn"));

    await screen.findByTestId("ports-error");
    expect(
      fetchMock.mock.calls.some((c) => (c[1] as RequestInit | undefined)?.method === "POST"),
    ).toBe(false);
  });
});
