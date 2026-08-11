import { afterEach, describe, expect, test } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { TokensPanel } from "./TokensPanel";

afterEach(() => cleanup());

describe("Credentials OAuth controls", () => {
  test("settles a successful reconnect without re-invalidating its own flow query", async () => {
    const originalFetch = global.fetch;
    let connected = true;
    let credentialVersion = 1;
    let flowPolls = 0;
    global.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
      const path = String(input);
      if (path.endsWith("/me/credentials") && !init?.method) {
        return Response.json({
          credentials: [
            {
              kind: "oauth",
              provider: "openai-codex",
              harnesses: [{ name: "codex", label: "Codex" }],
              connected,
              version: credentialVersion,
              account: { displayName: "person@example.com", planType: "plus" },
            },
          ],
        });
      }
      if (path.endsWith("/me/harness-env")) return Response.json({ vars: [] });
      if (path.endsWith("/me/credentials/openai-codex/connect") && init?.method === "POST") {
        return Response.json({
          flow: { id: "flow-success", provider: "openai-codex", status: "pending" },
          verificationUrl: "https://auth.openai.test/device",
          userCode: "ABCD-EFGH",
        });
      }
      if (path.endsWith("/me/credentials/flows/flow-success")) {
        flowPolls += 1;
        if (flowPolls === 1) {
          return Response.json({
            flow: { id: "flow-success", provider: "openai-codex", status: "pending" },
          });
        }
        connected = true;
        credentialVersion = 2;
        return Response.json({
          flow: { id: "flow-success", provider: "openai-codex", status: "succeeded" },
        });
      }
      throw new Error(`unexpected fetch ${path}`);
    }) as typeof fetch;
    try {
      renderWithProviders(<TokensPanel />);
      const user = userEvent.setup();
      await user.click(await screen.findByRole("button", { name: "Reconnect" }));
      expect(await screen.findByText("Waiting for OpenAI…")).toBeTruthy();
      await waitFor(() => expect(screen.queryByText("Waiting for OpenAI…")).toBeNull(), {
        timeout: 4_000,
      });
      await new Promise((resolve) => setTimeout(resolve, 200));
      expect(flowPolls).toBe(2);
      expect(screen.getByText(/person@example.com · plus/)).toBeTruthy();
    } finally {
      global.fetch = originalFetch;
    }
  });

  test("connects, polls a failed flow, and allows retry", async () => {
    const originalFetch = global.fetch;
    let starts = 0;
    global.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
      const path = String(input);
      if (path.endsWith("/me/credentials") && !init?.method) {
        return Response.json({
          credentials: [
            {
              kind: "oauth",
              provider: "openai-codex",
              harnesses: [{ name: "codex", label: "Codex" }],
              connected: false,
            },
          ],
        });
      }
      if (path.endsWith("/me/harness-env")) return Response.json({ vars: [] });
      if (path.endsWith("/me/credentials/openai-codex/connect") && init?.method === "POST") {
        starts += 1;
        return Response.json({
          flow: { id: `flow-${starts}`, provider: "openai-codex", status: "pending" },
          verificationUrl: "https://auth.openai.test/device",
          userCode: "ABCD-EFGH",
        });
      }
      if (path.includes("/me/credentials/flows/flow-")) {
        await new Promise((resolve) => setTimeout(resolve, 50));
        return Response.json({
          flow: {
            id: `flow-${starts}`,
            provider: "openai-codex",
            status: "failed",
            errorCode: "authorization_denied",
          },
        });
      }
      throw new Error(`unexpected fetch ${path}`);
    }) as typeof fetch;
    try {
      renderWithProviders(<TokensPanel />);
      const user = userEvent.setup();
      await user.click(await screen.findByRole("button", { name: "Connect" }));
      expect(await screen.findByText("ABCD-EFGH")).toBeTruthy();
      expect(await screen.findByText(/Connection failed/i, {}, { timeout: 3_000 })).toBeTruthy();
      await user.click(screen.getByRole("button", { name: "Connect" }));
      await waitFor(() => expect(starts).toBe(2));
    } finally {
      global.fetch = originalFetch;
    }
  });

  test("disconnects an existing OAuth connection", async () => {
    const originalFetch = global.fetch;
    let disconnected = false;
    global.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
      const path = String(input);
      if (path.endsWith("/me/credentials") && !init?.method) {
        return Response.json({
          credentials: [
            {
              kind: "oauth",
              provider: "openai-codex",
              harnesses: [{ name: "codex", label: "Codex" }],
              connected: true,
              account: { displayName: "person@example.com", planType: "plus" },
            },
          ],
        });
      }
      if (path.endsWith("/me/harness-env")) return Response.json({ vars: [] });
      if (path.endsWith("/me/credentials/openai-codex") && init?.method === "DELETE") {
        disconnected = true;
        return new Response(null, { status: 204 });
      }
      throw new Error(`unexpected fetch ${path}`);
    }) as typeof fetch;
    try {
      renderWithProviders(<TokensPanel />);
      const user = userEvent.setup();
      expect(await screen.findByText(/person@example.com/)).toBeTruthy();
      await user.click(screen.getByRole("button", { name: "Disconnect" }));
      await waitFor(() => expect(disconnected).toBe(true));
    } finally {
      global.fetch = originalFetch;
    }
  });
});

// ADR 0115: personal connector credentials — the Integration credentials
// section, its PAT form, and disconnect.
describe("Credentials connector cards", () => {
  const connectorEntry = (over: Record<string, unknown> = {}) => ({
    kind: "connector",
    provider: "acme",
    display: { name: "Acme" },
    modes: { oauth: true, token: true },
    tokenHint: "Create a PAT under Settings → API.",
    connected: false,
    status: "",
    ...over,
  });

  test("saves a pasted token through the connector PUT route", async () => {
    const originalFetch = global.fetch;
    const puts: Array<{ path: string; body: unknown }> = [];
    global.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
      const path = String(input);
      if (path.endsWith("/me/credentials") && !init?.method) {
        return Response.json({ credentials: [connectorEntry()] });
      }
      if (path.endsWith("/me/harness-env")) return Response.json({ vars: [] });
      if (path.endsWith("/me/connector-credentials/acme") && init?.method === "PUT") {
        puts.push({ path, body: JSON.parse(String(init.body)) });
        return new Response(null, { status: 204 });
      }
      throw new Error(`unexpected fetch ${path}`);
    }) as typeof fetch;
    try {
      renderWithProviders(<TokensPanel />);
      const user = userEvent.setup();
      expect(await screen.findByText("Integration credentials")).toBeTruthy();
      expect(screen.getByText(/Create a PAT under Settings → API/)).toBeTruthy();
      // OAuth connect is a plain browser navigation to the authorize route.
      expect(screen.getByRole("link", { name: "Connect" }).getAttribute("href")).toContain(
        "/me/connector-credentials/acme/oauth/authorize",
      );
      await user.click(screen.getByRole("button", { name: "Add token" }));
      await user.type(screen.getByPlaceholderText("paste token…"), "pat-secret");
      await user.click(screen.getByRole("button", { name: "Save" }));
      await waitFor(() => expect(puts).toHaveLength(1));
      expect(puts[0]!.body).toEqual({ value: "pat-secret" });
    } finally {
      global.fetch = originalFetch;
    }
  });

  test("disconnects a connected personal credential and surfaces a broken one", async () => {
    const originalFetch = global.fetch;
    let deleted = false;
    global.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
      const path = String(input);
      if (path.endsWith("/me/credentials") && !init?.method) {
        return Response.json({
          credentials: [
            connectorEntry({
              connected: true,
              status: deleted ? "" : "broken",
              version: 4,
              account: { displayName: "person@example.com" },
            }),
          ],
        });
      }
      if (path.endsWith("/me/harness-env")) return Response.json({ vars: [] });
      if (path.endsWith("/me/connector-credentials/acme") && init?.method === "DELETE") {
        deleted = true;
        return new Response(null, { status: 204 });
      }
      throw new Error(`unexpected fetch ${path}`);
    }) as typeof fetch;
    try {
      renderWithProviders(<TokensPanel />);
      const user = userEvent.setup();
      // A broken credential shows its status, not "connected".
      expect(await screen.findByText("broken")).toBeTruthy();
      await user.click(screen.getByRole("button", { name: "Disconnect" }));
      await waitFor(() => expect(deleted).toBe(true));
    } finally {
      global.fetch = originalFetch;
    }
  });
});
