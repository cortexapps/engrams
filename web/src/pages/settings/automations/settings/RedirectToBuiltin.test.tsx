import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen } from "@testing-library/react";
import { createRouterTransport, Code, ConnectError } from "@connectrpc/connect";

import { AutomationService } from "@/gen/engram/app/v1/automation_pb";
import { renderWithProviders } from "@/test-utils";
import { RedirectToBuiltin } from "./RedirectToBuiltin";

const navigated = vi.hoisted(() => vi.fn());
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  Navigate: (props: Record<string, unknown>) => {
    navigated(props);
    return null;
  },
}));

beforeEach(() => navigated.mockReset());

describe("RedirectToBuiltin", () => {
  it("navigates to the PR-review built-in's Inputs tab when it exists", async () => {
    const transport = createRouterTransport((router) => {
      router.service(AutomationService, {
        getAutomation: (req) => {
          expect(req.lookup).toEqual({ case: "builtinKey", value: "pr_review" });
          return { automation: { id: "auto-builtin", name: "PR review", kind: "builtin" } };
        },
      });
    });
    renderWithProviders(<RedirectToBuiltin />, { transport });
    await vi.waitFor(() =>
      expect(navigated).toHaveBeenCalledWith(
        expect.objectContaining({
          to: "/settings/automations/$id",
          params: { id: "auto-builtin" },
          search: { tab: "inputs" },
          replace: true,
        }),
      ),
    );
  });

  it("explains instead of bouncing when the built-in is not seeded yet", async () => {
    const transport = createRouterTransport((router) => {
      router.service(AutomationService, {
        getAutomation: () => {
          throw new ConnectError("no such built-in", Code.NotFound);
        },
      });
    });
    renderWithProviders(<RedirectToBuiltin />, { transport });
    expect(await screen.findByText(/reviewed repos moved/i)).toBeTruthy();
    expect(navigated).not.toHaveBeenCalled();
  });
});
