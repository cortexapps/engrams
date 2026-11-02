import { describe, expect, it } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { create } from "@bufbuild/protobuf";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "@/test-utils";
import { GetMcpSetupResponseSchema, IntegrationService } from "@/gen/engram/app/v1/integration_pb";
import { McpConnections } from "./McpConnections";
import { MCP_CALLBACK_PATH, saveMcpFlow } from "@/mcp-callback-bootstrap";

describe("external MCP connections", () => {
  it("offers only approved endpoints and creates disabled metadata", async () => {
    const requests: unknown[] = [];
    const transport = createRouterTransport((router) =>
      router.service(IntegrationService, {
        listConnections: () => ({ connections: [] }),
        createConnection: (req) => {
          requests.push(req);
          return {};
        },
      }),
    );
    renderWithProviders(
      <McpConnections
        setup={create(GetMcpSetupResponseSchema, {
          enabled: true,
          destinations: [{ zoneId: "public", endpoint: "https://tools.example/mcp" }],
        })}
      />,
      { transport },
    );
    const user = userEvent.setup();
    await user.type(await screen.findByLabelText("Connection name"), "Tools");
    await user.type(screen.getByLabelText("Connection alias"), "tools");
    await user.click(screen.getByRole("button", { name: "Create connection" }));
    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({
      alias: "tools",
      provider: "mcp",
      mcp: { zone: "public", endpoint: "https://tools.example/mcp", authKind: "api_key" },
    });
  });

  it("sends an API key directly and clears the input after failure", async () => {
    const requests: string[] = [];
    const connection = {
      id: "c1",
      provider: "mcp",
      alias: "tools",
      displayName: "Tools",
      configRevision: 1,
      authGeneration: 1,
      mcp: { endpoint: "https://tools.example/mcp", zone: "public", authKind: "api_key" },
    };
    const transport = createRouterTransport((router) =>
      router.service(IntegrationService, {
        listConnections: () => ({ connections: [connection] }),
        getMcpSetup: () => ({
          enabled: true,
          current: { connection, credential: { status: "not_configured" } },
        }),
        getMcpCatalogs: () => ({}),
        putMcpCredential: (req) => {
          requests.push(req.value);
          throw new Error("failed");
        },
      }),
    );
    const { queryClient } = renderWithProviders(
      <McpConnections
        setup={create(GetMcpSetupResponseSchema, {
          enabled: true,
          destinations: [{ zoneId: "public", endpoint: "https://tools.example/mcp" }],
        })}
      />,
      { transport },
    );
    const user = userEvent.setup();
    const input = (await screen.findByLabelText("API key for Tools")) as HTMLInputElement;
    await user.type(input, "key-secret");
    await user.click(screen.getByRole("button", { name: "Save key" }));
    await waitFor(() => expect(requests).toEqual(["key-secret"]));
    await waitFor(() => expect(input.value).toBe(""));
    expect(queryClient.getMutationCache().getAll()).toHaveLength(0);
    expect(JSON.stringify(sessionStorage)).not.toContain("key-secret");
  });
  it("does not start another OAuth flow while this tab has one", async () => {
    const redirectUri = `${window.location.origin}${MCP_CALLBACK_PATH}`;
    saveMcpFlow(sessionStorage, {
      connectionId: "other",
      flowId: "f1",
      configRevision: 1,
      authGeneration: 1,
      redirectUri,
      expiresAt: "2099-01-01T00:00:00Z",
    });
    const connection = {
      id: "c1",
      provider: "mcp",
      alias: "tools",
      displayName: "Tools",
      configRevision: 1,
      authGeneration: 1,
      mcp: {
        endpoint: "https://tools.example/mcp",
        zone: "public",
        authKind: "oauth",
        oauthClientRegistrationId: "reg",
        shareWithOrgSessions: true,
      },
    };
    let calls = 0;
    const transport = createRouterTransport((router) =>
      router.service(IntegrationService, {
        listConnections: () => ({ connections: [connection] }),
        getMcpSetup: () => ({ enabled: true, current: { connection } }),
        getMcpCatalogs: () => ({}),
        authorizeMcp: () => {
          calls++;
          return {};
        },
      }),
    );
    try {
      renderWithProviders(
        <McpConnections
          setup={create(GetMcpSetupResponseSchema, {
            enabled: true,
            destinations: [{ zoneId: "public", endpoint: "https://tools.example/mcp" }],
            registrations: [
              {
                id: "reg",
                redirectUri,
                resource: { zoneId: "public", endpoint: "https://tools.example/mcp" },
              },
            ],
          })}
        />,
        { transport },
      );
      const user = userEvent.setup();
      await user.click(await screen.findByRole("button", { name: "Authorize" }));
      await screen.findByRole("alert");
      expect(calls).toBe(0);
      expect(sessionStorage.length).toBe(1);
    } finally {
      sessionStorage.clear();
    }
  });
  it("keeps existing connections visible and marks unavailable actions when disabled", async () => {
    const connection = {
      id: "c1",
      provider: "mcp",
      alias: "tools",
      displayName: "Tools",
      mcp: { endpoint: "https://tools.example/mcp", zone: "public", authKind: "api_key" },
    };
    const transport = createRouterTransport((router) =>
      router.service(IntegrationService, {
        listConnections: () => ({ connections: [connection] }),
        getMcpSetup: () => ({ enabled: false, current: { connection } }),
        getMcpCatalogs: () => ({}),
      }),
    );
    renderWithProviders(
      <McpConnections setup={create(GetMcpSetupResponseSchema, { enabled: false })} />,
      { transport },
    );
    expect(await screen.findByRole("region", { name: "Tools MCP connection" })).toBeTruthy();
    expect((screen.getByRole("button", { name: "Recover" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect(screen.getByText(/actions and recovery are unavailable/i)).toBeTruthy();
  });
});
