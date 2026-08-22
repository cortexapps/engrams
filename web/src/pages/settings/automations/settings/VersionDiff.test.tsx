import { describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";

import { AutomationService } from "@/gen/engram/app/v1/automation_pb";
import { renderWithProviders } from "@/test-utils";
import { definitionDiff, VersionDiff } from "./VersionDiff";
import { VersionsList, versionLabel } from "./VersionsList";

const V1 = JSON.stringify({
  blocks: [{ id: "a", type: "filter" }],
  settings: { endSessionsOnFinish: false },
});
const V2 = JSON.stringify({
  blocks: [{ id: "a", type: "filter" }],
  settings: { endSessionsOnFinish: true },
});

describe("definitionDiff", () => {
  it("reports only the changed lines of the pretty-printed definitions", () => {
    const changes = definitionDiff(V1, V2);
    const removed = changes.filter((c) => c.removed).map((c) => c.value.trim());
    const added = changes.filter((c) => c.added).map((c) => c.value.trim());
    expect(removed).toEqual(['"endSessionsOnFinish": false']);
    expect(added).toEqual(['"endSessionsOnFinish": true']);
  });

  it("renders identical definitions as such", async () => {
    renderWithProviders(
      <VersionDiff beforeLabel="v1" afterLabel="v2" beforeJson={V1} afterJson={V1} />,
    );
    expect((await screen.findByTestId("version-diff")).textContent).toContain(
      "identical definitions",
    );
  });
});

describe("VersionsList", () => {
  it("labels built-in versions as shipped and diffs the two picked versions", async () => {
    expect(versionLabel(3, true)).toBe("shipped v3");
    expect(versionLabel(3, false)).toBe("v3");
    const transport = createRouterTransport((router) => {
      router.service(AutomationService, {
        listVersions: () => ({
          versions: [
            { automationId: "a", number: 1, definitionJson: V1, createdAt: "2026-08-21T00:00:00Z" },
            { automationId: "a", number: 2, definitionJson: V2, createdAt: "2026-08-21T01:00:00Z" },
          ],
        }),
      });
    });
    renderWithProviders(<VersionsList automationId="a" builtin={false} />, { transport });
    const user = userEvent.setup();
    await user.click(await screen.findByTestId("version-pick-2"));
    await user.click(screen.getByTestId("version-pick-1"));
    expect((await screen.findByTestId("version-diff")).getAttribute("aria-label")).toBe(
      "diff v1 to v2",
    );
    expect(screen.getAllByText(/"endSessionsOnFinish": true/).length).toBeGreaterThan(0);
  });
});
