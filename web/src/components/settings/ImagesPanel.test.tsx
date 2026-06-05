// Regression tests for the ImagesPanel wire contract.
//
// Mirrors the structure of RegistriesPanel.test.tsx: mock fetch with
// a route-aware stub, drive the panel via user events, assert on the
// captured POST body. Catches silent drift in the JSON shape we send
// to /api/enabled-images — a refactor that ships `uri` instead of
// `image_uri`, or routes /disable as a DELETE, would slip past the
// Rust integration test.

import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { ImagesPanel } from "./ImagesPanel";

interface FetchCall {
  url: string;
  method: string;
  body?: string;
}

/** Install a route-aware fetch stub. Returns the spy + a snapshot
 * accessor so each test reads its own POST history. */
function installFetchMock(initialList: unknown[] = [], initialJobs: unknown[] = []) {
  let listSnapshot = initialList;
  let jobsSnapshot = initialJobs;
  const spy = vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url =
        typeof input === "string"
          ? input
          : input instanceof URL
            ? input.toString()
            : (input as Request).url;
      const method = init?.method ?? "GET";

      if (url === "/api/v1/enabled-images" && method === "GET") {
        return new Response(JSON.stringify({ images: listSnapshot }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/v1/enable-jobs" && method === "GET") {
        return new Response(JSON.stringify({ jobs: jobsSnapshot }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/v1/enabled-images" && method === "POST") {
        return new Response(
          JSON.stringify({
            id: "00000000-0000-0000-0000-000000000000",
            image_uri: "ghcr.io/cortex/api:warm-1",
            manifest_digest: "sha256:abc",
            manifest_name: "cortex-api",
            manifest_description: null,
            harness_name: null,
            last_refreshed_at: new Date().toISOString(),
            created_at: new Date().toISOString(),
          }),
          { status: 201, headers: { "content-type": "application/json" } },
        );
      }
      if (url === "/api/v1/enabled-images/disable" && method === "POST") {
        return new Response(null, { status: 204 });
      }
      if (url === "/api/v1/enabled-images/refresh" && method === "POST") {
        return new Response(
          JSON.stringify({
            id: "00000000-0000-0000-0000-000000000000",
            image_uri: "ghcr.io/cortex/api:warm-1",
            manifest_digest: "sha256:def",
            manifest_name: "cortex-api",
            manifest_description: null,
            harness_name: null,
            last_refreshed_at: new Date().toISOString(),
            created_at: new Date().toISOString(),
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }
      throw new Error(`unexpected fetch in test: ${method} ${url}`);
    });
  return {
    spy,
    setList: (rows: unknown[]) => {
      listSnapshot = rows;
    },
    setJobs: (rows: unknown[]) => {
      jobsSnapshot = rows;
    },
    callsMatching(pred: (c: FetchCall) => boolean): FetchCall[] {
      return spy.mock.calls
        .map(([input, init]) => {
          const u =
            typeof input === "string"
              ? input
              : input instanceof URL
                ? input.toString()
                : (input as Request).url;
          return {
            url: u,
            method: init?.method ?? "GET",
            body: typeof init?.body === "string" ? init?.body : undefined,
          };
        })
        .filter(pred);
    },
  };
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ImagesPanel wire contract", () => {
  test("enable form posts {image_uri} to /api/enabled-images", async () => {
    const mock = installFetchMock([]);
    renderWithProviders(<ImagesPanel />);

    const user = userEvent.setup();
    const trigger = await screen.findByRole("button", {
      name: /enable a new image/i,
    });
    await user.click(trigger);

    await user.type(
      screen.getByPlaceholderText("ghcr.io/cortex/api:warm-1"),
      "ghcr.io/cortex/api:warm-1",
    );
    await user.click(screen.getByRole("button", { name: /^enable$/i }));

    await waitFor(() => {
      const posts = mock.callsMatching(
        (c) => c.method === "POST" && c.url === "/api/v1/enabled-images",
      );
      expect(posts.length).toBeGreaterThan(0);
      expect(JSON.parse(posts.at(-1)!.body!)).toEqual({
        image_uri: "ghcr.io/cortex/api:warm-1",
      });
    });
  });

  test("disable button posts URI in body to /disable (not in path)", async () => {
    // The image URI contains slashes + colons, which makes it
    // awkward as a path segment. The wire shape uses POST + body
    // instead of DELETE + path. Lock that here.
    const mock = installFetchMock([
      {
        id: "row-1",
        image_uri: "ghcr.io/cortex/api:warm-1",
        manifest_digest: "sha256:abc",
        manifest_name: "cortex-api",
        manifest_description: null,
        harness_name: null,
        last_refreshed_at: new Date().toISOString(),
        created_at: new Date().toISOString(),
      },
    ]);
    renderWithProviders(<ImagesPanel />);

    const user = userEvent.setup();
    // Wait for the row to render.
    await screen.findByText("ghcr.io/cortex/api:warm-1");

    await user.click(screen.getByRole("button", { name: /^disable$/i }));
    // Confirmation prompt (shadcn AlertDialog) — the confirm action is labelled
    // "Disable image" to disambiguate it from the row's "Disable" trigger.
    await user.click(await screen.findByRole("button", { name: /disable image/i }));

    await waitFor(() => {
      const posts = mock.callsMatching(
        (c) => c.method === "POST" && c.url === "/api/v1/enabled-images/disable",
      );
      expect(posts.length).toBe(1);
      expect(JSON.parse(posts[0].body!)).toEqual({
        image_uri: "ghcr.io/cortex/api:warm-1",
      });
    });
  });

  test("refresh button posts URI to /refresh", async () => {
    const mock = installFetchMock([
      {
        id: "row-1",
        image_uri: "ghcr.io/cortex/api:warm-1",
        manifest_digest: "sha256:abc",
        manifest_name: "cortex-api",
        manifest_description: null,
        harness_name: null,
        last_refreshed_at: new Date().toISOString(),
        created_at: new Date().toISOString(),
      },
    ]);
    renderWithProviders(<ImagesPanel />);

    const user = userEvent.setup();
    await screen.findByText("ghcr.io/cortex/api:warm-1");
    await user.click(screen.getByRole("button", { name: /^refresh$/i }));

    await waitFor(() => {
      const posts = mock.callsMatching(
        (c) => c.method === "POST" && c.url === "/api/v1/enabled-images/refresh",
      );
      expect(posts.length).toBe(1);
      expect(JSON.parse(posts[0].body!)).toEqual({
        image_uri: "ghcr.io/cortex/api:warm-1",
      });
    });
  });

  test("active refresh job suppresses the static image row for the same URI", async () => {
    // Both an enabled-images row and an active enable-job exist for the
    // same URI (the state right after clicking "refresh"). The panel must
    // show only the progress row — not both.
    installFetchMock(
      [
        {
          id: "row-1",
          image_uri: "ghcr.io/cortex/api:warm-1",
          manifest_digest: "sha256:abc",
          manifest_name: "cortex-api",
          manifest_description: null,
          harness_name: null,
          last_refreshed_at: new Date().toISOString(),
          created_at: new Date().toISOString(),
        },
      ],
      [
        {
          id: "job-1",
          image_uri: "ghcr.io/cortex/api:warm-1",
          state: "materializing",
          chunks_done: 3,
          chunks_total: 10,
          error: null,
          created_at: new Date().toISOString(),
          updated_at: new Date().toISOString(),
        },
      ],
    );
    renderWithProviders(<ImagesPanel />);

    // The URI should appear exactly once — from the EnableJobRow.
    await waitFor(() => {
      const matches = screen.getAllByText("ghcr.io/cortex/api:warm-1");
      expect(matches).toHaveLength(1);
    });
  });
});
