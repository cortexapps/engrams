// web-H2: connection delete must confirm first and must surface the server's
// rejection (delete of a still-granted connection) instead of doing nothing.
// The enable path must surface its failure too.

import { afterEach, describe, expect, test } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Code, ConnectError, createRouterTransport } from "@connectrpc/connect";

import { renderWithProviders } from "../../test-utils";
import { GoogleCloudConnections } from "./GoogleCloudConnections";
import {
  IntegrationService,
  type IntegrationConnection,
} from "../../gen/engram/app/v1/integration_pb";

const CONNECTION = {
  id: "connection-1",
  alias: "prod-readonly",
  provider: "gcp",
  displayName: "Production read only",
  enabled: false,
  testedAt: "2026-08-01T00:00:00Z",
  createdAt: "",
  updatedAt: "",
  googleCloud: {
    workloadIdentityProvider:
      "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/oidc",
    serviceAccountEmail: "reader@customer.iam.gserviceaccount.com",
    endpoints: ["logging.googleapis.com"],
  },
} as IntegrationConnection;

function installTransport(options: { deleteError?: ConnectError; enableError?: ConnectError }) {
  const deletes: string[] = [];
  const transport = createRouterTransport((router) => {
    router.service(IntegrationService, {
      listConnections: () => ({ connections: [CONNECTION] }),
      deleteConnection: (req) => {
        if (options.deleteError) throw options.deleteError;
        deletes.push(req.id);
        return { deleted: true };
      },
      setConnectionEnabled: () => {
        if (options.enableError) throw options.enableError;
        return { connection: undefined };
      },
      testConnection: () => ({ ok: true, message: "STS and impersonation passed" }),
    });
  });
  return { transport, deletes };
}

describe("GoogleCloudConnections", () => {
  afterEach(() => cleanup());

  test("delete asks for confirmation before it calls the server", async () => {
    const { transport, deletes } = installTransport({});
    renderWithProviders(<GoogleCloudConnections />, { transport });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /delete production read only/i }));
    expect(deletes).toHaveLength(0);
    expect(screen.getByText(/delete "production read only"\?/i)).toBeTruthy();

    await user.click(screen.getByRole("button", { name: /^cancel$/i }));
    expect(screen.queryByText(/delete "production read only"\?/i)).toBeNull();
    expect(deletes).toHaveLength(0);

    await user.click(screen.getByRole("button", { name: /delete production read only/i }));
    await user.click(screen.getByRole("button", { name: /^delete$/i }));
    await waitFor(() => expect(deletes).toEqual(["connection-1"]));
  });

  test("a rejected delete surfaces the server's message on the row", async () => {
    const { transport } = installTransport({
      deleteError: new ConnectError(
        'connection "prod-readonly" is still granted by profile "Backend Agent"',
        Code.FailedPrecondition,
      ),
    });
    renderWithProviders(<GoogleCloudConnections />, { transport });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /delete production read only/i }));
    await user.click(screen.getByRole("button", { name: /^delete$/i }));
    expect(await screen.findByText(/still granted by profile "Backend Agent"/)).toBeTruthy();
    // The confirm bar stays open so the user can retry after fixing profiles.
    expect(screen.getByText(/delete "production read only"\?/i)).toBeTruthy();
  });

  test("an enable failure surfaces instead of doing nothing", async () => {
    const { transport } = installTransport({
      enableError: new ConnectError("test the connection first", Code.FailedPrecondition),
    });
    renderWithProviders(<GoogleCloudConnections />, { transport });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /^enable$/i }));
    expect(await screen.findByText(/test the connection first/)).toBeTruthy();
  });
});
