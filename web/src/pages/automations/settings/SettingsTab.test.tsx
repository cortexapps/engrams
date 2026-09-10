import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Code, ConnectError, createRouterTransport } from "@connectrpc/connect";

import {
  AutomationService,
  type ArchiveAutomationRequest,
  type UpdateAutomationMetaRequest,
} from "@/gen/engram/app/v1/automation_pb";
import { renderWithProviders } from "@/test-utils";
import { SettingsTab, settingsFromDraft } from "./SettingsTab";

const navigate = vi.hoisted(() => vi.fn());
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => navigate,
}));
const toastError = vi.hoisted(() => vi.fn());
vi.mock("sonner", async (orig) => {
  const real = await orig<typeof import("sonner")>();
  return { ...real, toast: { ...real.toast, error: toastError } };
});

const T0 = "2026-08-21T12:00:00Z";

function automation(kind: "user" | "builtin") {
  return {
    id: "auto-1",
    name: kind === "builtin" ? "PR review" : "Nightly triage",
    description: "",
    kind,
    ...(kind === "builtin" ? { builtinKey: "pr_review" } : {}),
    enabled: true,
    currentVersion: 2,
    inputsJson: "{}",
    blockOverridesJson: "{}",
    version: {
      automationId: "auto-1",
      number: 2,
      definitionJson: JSON.stringify({
        engine: 1,
        trigger: { kind: "manual" },
        blocks: [],
        inputsSchema: [],
        settings: {
          concurrency: { keyTemplate: "${{ event.pr.url }}", policy: "supersede" },
          runDeadlineSeconds: 600,
          endSessionsOnFinish: true,
        },
      }),
      createdAt: T0,
    },
    createdAt: T0,
    updatedAt: T0,
  };
}

// Loose impl types: the router transport accepts MessageInit shapes, and the
// test only inspects the request.
type MetaImpl = (req: UpdateAutomationMetaRequest) => { automation: ReturnType<typeof automation> };
type ArchiveImpl = (req: ArchiveAutomationRequest) => { automation: ReturnType<typeof automation> };

function transportFor(
  kind: "user" | "builtin",
  stubs: { updateAutomationMeta?: MetaImpl; archiveAutomation?: ArchiveImpl } = {},
) {
  const row = automation(kind);
  return createRouterTransport((router) => {
    router.service(AutomationService, {
      getAutomation: () => ({ automation: row }),
      listVersions: () => ({
        versions: [
          { automationId: "auto-1", number: 1, definitionJson: "{}", createdAt: T0 },
          {
            automationId: "auto-1",
            number: 2,
            definitionJson: row.version.definitionJson,
            createdAt: T0,
          },
        ],
      }),
      updateAutomationMeta: stubs.updateAutomationMeta ?? (() => ({ automation: row })),
      archiveAutomation: stubs.archiveAutomation ?? (() => ({ automation: row })),
      duplicateAutomation: () => ({ automation: { ...row, id: "auto-copy", kind: "user" } }),
    });
  });
}

beforeEach(() => {
  navigate.mockReset();
  toastError.mockReset();
});

describe("settingsFromDraft", () => {
  it("builds settings and validates the key + deadline", () => {
    expect(
      settingsFromDraft({
        policy: "queue",
        keyTemplate: " ${{ x }} ",
        deadlineMinutes: "15",
        endSessionsOnFinish: false,
      }).settings,
    ).toEqual({
      concurrency: { keyTemplate: "${{ x }}", policy: "queue" },
      runDeadlineSeconds: 900,
      endSessionsOnFinish: false,
    });
    expect(
      settingsFromDraft({
        policy: "queue",
        keyTemplate: "",
        deadlineMinutes: "",
        endSessionsOnFinish: true,
      }).error,
    ).toMatch(/key template/);
    expect(
      settingsFromDraft({
        policy: "__none__",
        keyTemplate: "",
        deadlineMinutes: "0.5",
        endSessionsOnFinish: true,
      }).error,
    ).toMatch(/deadline/);
    expect(
      settingsFromDraft({
        policy: "__none__",
        keyTemplate: "",
        deadlineMinutes: "",
        endSessionsOnFinish: true,
      }).settings,
    ).toEqual({ endSessionsOnFinish: true });
  });
});

describe("SettingsTab", () => {
  it("saves changed settings as settings_json on a user automation", async () => {
    const updateAutomationMeta = vi.fn<MetaImpl>(() => ({ automation: automation("user") }));
    renderWithProviders(<SettingsTab automationId="auto-1" />, {
      transport: transportFor("user", { updateAutomationMeta }),
    });
    const deadline = await screen.findByLabelText(/run deadline/i);
    expect((deadline as HTMLInputElement).value).toBe("10");
    const user = userEvent.setup();
    await user.clear(deadline);
    await user.type(deadline, "30");
    await user.click(screen.getByRole("button", { name: /save settings/i }));
    await waitFor(() => expect(updateAutomationMeta).toHaveBeenCalled());
    const req = updateAutomationMeta.mock.calls[0]![0];
    expect(req.id).toBe("auto-1");
    expect(JSON.parse(req.settingsJson!)).toEqual({
      concurrency: { keyTemplate: "${{ event.pr.url }}", policy: "supersede" },
      runDeadlineSeconds: 1800,
      endSessionsOnFinish: true,
    });
  });

  it("renders settings read-only on a built-in, with the hint, and disables Archive", async () => {
    renderWithProviders(<SettingsTab automationId="auto-1" />, {
      transport: transportFor("builtin"),
    });
    const deadline = await screen.findByLabelText(/run deadline/i);
    expect((deadline as HTMLInputElement).disabled).toBe(true);
    expect(screen.getAllByText(/set by the built-in/i).length).toBeGreaterThan(0);
    expect(screen.queryByRole("button", { name: /save settings/i })).toBeNull();
    expect((screen.getByRole("button", { name: /^archive$/i }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    // Built-in versions read "shipped vN".
    expect((await screen.findByTestId("version-pick-2")).textContent).toBe("shipped v2");
  });

  it("archives after confirmation and returns to the list", async () => {
    const archiveAutomation = vi.fn<ArchiveImpl>(() => ({ automation: automation("user") }));
    renderWithProviders(<SettingsTab automationId="auto-1" />, {
      transport: transportFor("user", { archiveAutomation }),
    });
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /^archive$/i }));
    // Opening the dialog is not archiving.
    expect(archiveAutomation).not.toHaveBeenCalled();
    const dialog = await screen.findByRole("alertdialog");
    await user.click(within(dialog).getByRole("button", { name: /^archive$/i }));
    await waitFor(() =>
      expect(archiveAutomation).toHaveBeenCalledWith(
        expect.objectContaining({ id: "auto-1" }),
        expect.anything(),
      ),
    );
    await waitFor(() =>
      expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ to: "/automations" })),
    );
  });

  it("a failed archive surfaces the error and never navigates away", async () => {
    // The confirm dialog closes on click regardless of outcome, so a swallowed
    // rejection would leave the operator believing the automation is gone
    // while it keeps firing.
    const archiveAutomation = vi.fn<ArchiveImpl>(() => {
      throw new ConnectError("another admin is archiving this automation", Code.Aborted);
    });
    renderWithProviders(<SettingsTab automationId="auto-1" />, {
      transport: transportFor("user", { archiveAutomation }),
    });
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /^archive$/i }));
    const dialog = await screen.findByRole("alertdialog");
    await user.click(within(dialog).getByRole("button", { name: /^archive$/i }));
    await waitFor(() => expect(archiveAutomation).toHaveBeenCalled());
    await waitFor(() =>
      expect(toastError).toHaveBeenCalledWith(expect.stringMatching(/another admin is archiving/)),
    );
    expect(navigate).not.toHaveBeenCalled();
  });

  it("duplicates and opens the copy in the editor", async () => {
    renderWithProviders(<SettingsTab automationId="auto-1" />, {
      transport: transportFor("builtin"),
    });
    await userEvent.setup().click(await screen.findByRole("button", { name: /duplicate/i }));
    await waitFor(() =>
      expect(navigate).toHaveBeenCalledWith(
        expect.objectContaining({ params: { id: "auto-copy" }, search: { tab: "build" } }),
      ),
    );
  });
});
