import { describe, expect, it } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Code, ConnectError, createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "@/test-utils";
import { IntegrationService } from "@/gen/engram/app/v1/integration_pb";
import { McpCatalogReview } from "./McpCatalogReview";

describe("MCP catalog review", () => {
  it("reloads the stored draft, renders descriptions as text, and requires explicit classification", async () => {
    const published: unknown[] = [];
    const transport = createRouterTransport((router) =>
      router.service(IntegrationService, {
        getMcpCatalogs: () => ({
          configRevision: 1,
          catalogRevision: 2,
          draft: {
            id: "draft",
            connectionId: "c1",
            kind: "draft",
            revision: 0,
            configRevision: 1,
            authGeneration: 1,
            tools: [
              {
                upstreamName: "create.item",
                description: '<img src="https://example.test/tracker">',
                inputSchemaJson: '{"type":"object"}',
              },
            ],
          },
        }),
        publishMcp: (req) => {
          published.push(req);
          return {};
        },
      }),
    );
    renderWithProviders(
      <McpCatalogReview connectionId="c1" revision={1} authGeneration={1} discoveryAvailable />,
      { transport },
    );
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Review draft" }));
    expect(screen.getByText('<img src="https://example.test/tracker">')).toBeTruthy();
    expect(document.querySelector('img[src="https://example.test/tracker"]')).toBeNull();
    expect(
      (screen.getByRole("button", { name: "Publish reviewed catalog" }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);
    await user.selectOptions(screen.getByLabelText("Classification for create.item"), "write");
    await user.click(screen.getByRole("button", { name: "Publish reviewed catalog" }));
    await waitFor(() => expect(published).toHaveLength(1));
    expect(published[0]).toMatchObject({
      connectionId: "c1",
      draftId: "draft",
      expectedRevision: 2,
      tools: [{ upstreamName: "create.item", classification: "write", enabled: false }],
    });
  });
  it("drops a stale review and requires a new review after a publication conflict", async () => {
    let reads = 0;
    const transport = createRouterTransport((router) =>
      router.service(IntegrationService, {
        getMcpCatalogs: () => {
          reads++;
          return {
            configRevision: 1,
            catalogRevision: reads,
            draft: {
              id: "draft",
              configRevision: 1,
              authGeneration: 1,
              tools: [{ upstreamName: "read.item", inputSchemaJson: '{"type":"object"}' }],
            },
          };
        },
        publishMcp: () => {
          throw new ConnectError("stale", Code.Aborted);
        },
      }),
    );
    renderWithProviders(
      <McpCatalogReview connectionId="c1" revision={1} authGeneration={1} discoveryAvailable />,
      { transport },
    );
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Review draft" }));
    await user.selectOptions(screen.getByLabelText("Classification for read.item"), "read");
    await user.click(screen.getByRole("button", { name: "Publish reviewed catalog" }));
    expect(await screen.findByText(/reload and review the draft again/i)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Publish reviewed catalog" })).toBeNull();
    expect(reads).toBeGreaterThan(1);
  });
});
