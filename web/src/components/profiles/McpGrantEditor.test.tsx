import { create } from "@bufbuild/protobuf";
import { createRouterTransport } from "@connectrpc/connect";
import { useState } from "react";
import { expect, it } from "vitest";
import { fireEvent, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  IntegrationConnectionSchema,
  IntegrationService,
} from "@/gen/engram/app/v1/integration_pb";
import { renderWithProviders } from "@/test-utils";
import { McpGrantEditor } from "./McpGrantEditor";
import type { McpProfileGrant } from "./profileForm";

it("requires a constraint or explicit unrestricted choice and keeps documents separate", async () => {
  const connection = create(IntegrationConnectionSchema, {
    id: "c1",
    displayName: "Tools",
    provider: "mcp",
    configRevision: 1,
    authGeneration: 1,
    catalogRevision: 1,
  });
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getMcpCatalogs: () => ({
        catalogRevision: 1,
        published: {
          id: "p1",
          revision: 1,
          configRevision: 1,
          authGeneration: 1,
          tools: [{ upstreamName: "write.item", classification: "write", enabled: true }],
        },
      }),
    }),
  );
  function Fixture() {
    const [grants, setGrants] = useState<McpProfileGrant[]>([]);
    return (
      <>
        <McpGrantEditor
          connection={connection}
          grants={grants}
          onChange={setGrants}
          onAvailability={() => {}}
        />
        <output>{JSON.stringify(grants)}</output>
      </>
    );
  }
  renderWithProviders(<Fixture />, { transport });
  const user = userEvent.setup();
  await user.selectOptions(await screen.findByLabelText("Reviewed tool for Tools"), "write.item");
  expect(
    (screen.getByRole("button", { name: "Add MCP grant" }) as HTMLButtonElement).disabled,
  ).toBe(true);
  fireEvent.change(screen.getByLabelText("Argument constraint JSON"), {
    target: { value: '{"type":"object"}' },
  });
  await user.click(screen.getByRole("button", { name: "Add MCP grant" }));
  await user.click(screen.getByRole("button", { name: "Add constraint document" }));
  const output = JSON.parse(screen.getByRole("status").textContent ?? "[]");
  expect(output[0].mcp.argumentConstraintDocumentsJson).toEqual(['{"type":"object"}', ""]);
  expect(output[0].mcp.unrestricted).toBe(false);
});

it("does not offer tools or publication review for a stale catalog", async () => {
  const connection = create(IntegrationConnectionSchema, {
    id: "c1",
    displayName: "Tools",
    provider: "mcp",
    configRevision: 2,
    authGeneration: 1,
    catalogRevision: 1,
  });
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getMcpCatalogs: () => ({
        catalogRevision: 1,
        published: {
          id: "p1",
          revision: 1,
          configRevision: 1,
          authGeneration: 1,
          tools: [{ upstreamName: "write.item", classification: "write", enabled: true }],
        },
      }),
    }),
  );
  const reports: boolean[] = [];
  renderWithProviders(
    <McpGrantEditor
      connection={connection}
      grants={[]}
      onChange={() => {}}
      onAvailability={(_id, ready) => {
        reports.push(ready);
      }}
    />,
    { transport },
  );
  expect((await screen.findByRole("alert")).textContent).toContain(
    "current reviewed catalog is unavailable",
  );
  expect(screen.queryByLabelText("Reviewed tool for Tools")).toBeNull();
  expect(reports.at(-1)).toBe(false);
});
