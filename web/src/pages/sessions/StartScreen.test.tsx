// Contract test for the task start screen (the /sessions landing): preselects
// the last-launched profile, renders the collapsible "this session can" receipt,
// and launching calls CreateTask with the selected profile + prompt.

import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { StartScreen } from "./StartScreen";
import { TaskService } from "../../gen/engram/app/v1/task_pb";
import { ProfileService } from "../../gen/engram/app/v1/profile_pb";
import { IntegrationService } from "../../gen/engram/app/v1/integration_pb";
import { ImageService } from "../../gen/engram/app/v1/image_pb";
import { HarnessCatalogService } from "../../gen/engram/app/v1/harness_pb";

const BUGFIX = {
  id: "pf1",
  name: "Bug-fix agent",
  description: "Reproduce, fix, open a PR.",
  icon: "Bug",
  imageId: "img1",
  includeUserTokens: false,
  envVars: {},
  skills: [],
  capabilities: ["github:pulls:write"],
  secrets: [],
};
const DOCS = {
  id: "pf2",
  name: "Docs agent",
  description: "Edit docs, fully sandboxed.",
  icon: "FileText",
  imageId: "img1",
  includeUserTokens: false,
  envVars: {},
  skills: [],
  capabilities: [],
  secrets: [],
};
// No connector powers, but raw network egress to an internal host: capCount 0
// yet reachable > 0 — the receipt must still disclose it.
const NETONLY = {
  id: "pf3",
  name: "Egress agent",
  description: "Reaches an internal host, no connectors.",
  icon: "Server",
  imageId: "img1",
  includeUserTokens: false,
  envVars: {},
  skills: [],
  capabilities: [],
  network: { default: "deny", allowHosts: ["internal.acme.test"], allowHostPatterns: [] },
  secrets: [],
};
const CATALOG = [
  {
    provider: "github",
    credentialSource: "mint",
    hosts: ["api.github.com"],
    display: {
      name: "GitHub",
      category: "Source control",
      blurb: "",
      icon: { mono: "GH", color: "#1f2328", logo: "" },
    },
    capabilities: [{ action: "pulls:write", access: "write", asset: "pull_request" }],
  },
];

function installTransport(opts: { harnessUserEnv?: boolean } = {}) {
  const created: Array<{ type: string; profileId: string; prompt?: string }> = [];
  const transport = createRouterTransport((router) => {
    // The harness catalog is only wired when a test needs the user-env
    // block: a single "claude" harness declaring a user credential + setup hint.
    if (opts.harnessUserEnv) {
      router.service(HarnessCatalogService, {
        listHarnesses: () => ({
          harnesses: [
            {
              name: "claude",
              builtIn: true,
              descriptor: {
                name: "claude",
                label: "Claude Code",
                auth: {
                  userEnv: "CLAUDE_CODE_OAUTH_TOKEN",
                  userEnvHint: "Run `claude setup-token`.",
                },
                models: [],
                effort: [],
              },
            },
          ],
        }),
        getHarness: () => ({ harness: undefined }),
        registerHarness: () => ({ harness: undefined }),
        deleteHarness: () => ({ deleted: false }),
      });
    }
    router.service(TaskService, {
      createTask: (req) => {
        created.push({ type: req.type, profileId: req.profileId, prompt: req.prompt });
        return {
          task: {
            id: "t1",
            type: "chat",
            title: "",
            status: "open",
            sessions: [{ sessionId: "s1" }],
          },
        };
      },
      listTasks: () => ({ tasks: [] }),
      getTask: () => ({ task: undefined }),
      deleteTask: () => ({}),
    });
    router.service(ProfileService, {
      listProfiles: () => ({ profiles: [BUGFIX, DOCS, NETONLY] }),
      getProfile: () => ({ profile: undefined }),
      createProfile: () => ({ profile: undefined }),
      updateProfile: () => ({ profile: undefined }),
      deleteProfile: () => ({}),
    });
    router.service(IntegrationService, {
      getIntegrationCatalog: () => ({ providers: CATALOG }),
      listConnectors: () => ({ connectors: [] }),
      upsertConnector: () => ({ connector: undefined }),
      deleteConnector: () => ({ deleted: true }),
      setMintCredential: () => ({ secretNames: [] }),
      uploadConnectorLogo: () => ({ logoUrl: "" }),
    });
    router.service(ImageService, {
      listEnabledImages: () => ({ images: [{ id: "img1", imageUri: "registry/api:warm-1" }] }),
      enableImage: () => ({ job: undefined }),
      disableImage: () => ({}),
      refreshImage: () => ({ job: undefined }),
      listEnableJobs: () => ({ jobs: [] }),
      getEnableJob: () => ({ job: undefined }),
      retryEnableJob: () => ({ job: undefined }),
      listRegistries: () => ({ registries: [] }),
      addRegistry: () => ({ id: "", host: "", authKind: "", authPrincipal: undefined }),
      deleteRegistry: () => ({}),
    });
  });
  return { transport, created };
}

describe("StartScreen", () => {
  beforeEach(() => localStorage.clear());
  afterEach(() => {
    cleanup();
    localStorage.clear();
  });

  test("preselects the first profile and launches with the prompt", async () => {
    const { transport, created } = installTransport();
    renderWithProviders(<StartScreen />, { transport });
    const user = userEvent.setup();

    // No stored preference → first profile is the default, shown in the switcher.
    expect(await screen.findByText("Bug-fix agent")).toBeTruthy();

    // The receipt summary is visible; expanding reveals the derived power + host.
    await user.click(screen.getByText(/this session can/i));
    expect(await screen.findByText(/write pulls/i)).toBeTruthy();
    expect(screen.getByText("api.github.com")).toBeTruthy();

    await user.type(screen.getByLabelText("Task"), "Fix the flaky test and open a PR.");
    await user.click(screen.getByTestId("launch-task"));

    await waitFor(() => expect(created).toHaveLength(1));
    expect(created[0]).toEqual({
      type: "chat",
      profileId: "pf1",
      prompt: "Fix the flaky test and open a PR.",
    });
  });

  test("preselects the last-launched profile from storage", async () => {
    localStorage.setItem("engrams:lastProfileId", "pf2");
    const { transport } = installTransport();
    renderWithProviders(<StartScreen />, { transport });

    // The stored profile wins over the first-in-list default.
    expect(await screen.findByText("Docs agent")).toBeTruthy();
    // Docs grants no powers and no egress — every session is sandboxed, so
    // there's nothing to disclose and the reach receipt doesn't render.
    expect(screen.queryByText(/this session can reach/i)).toBeNull();
  });

  // A harness that declares a user credential blocks launch until the
  // user sets it — the create surface shows the requirement + setup hint instead
  // of silently launching an un-authed session.
  test("blocks launch and shows the setup hint when the harness user credential is unset", async () => {
    const origFetch = global.fetch;
    global.fetch = (async (url: string | URL | Request) => {
      if (String(url).endsWith("/me/harness-env")) {
        return new Response(
          JSON.stringify({
            vars: [
              {
                envVar: "CLAUDE_CODE_OAUTH_TOKEN",
                harnesses: [{ name: "claude", label: "Claude Code" }],
                hint: "Run `claude setup-token`.",
                present: false,
              },
            ],
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }
      throw new Error(`unexpected fetch ${String(url)}`);
    }) as typeof fetch;
    try {
      const { transport, created } = installTransport({ harnessUserEnv: true });
      renderWithProviders(<StartScreen />, { transport });
      const user = userEvent.setup();

      expect(await screen.findByText("Bug-fix agent")).toBeTruthy();
      // The blocker names the missing env var and surfaces the descriptor's hint.
      expect(await screen.findByText(/sessions need it to launch/i)).toBeTruthy();
      expect(screen.getByText(/claude setup-token/i)).toBeTruthy();

      // Even with a prompt typed, launch stays disabled and creates nothing.
      await user.type(screen.getByLabelText("Task"), "Do the thing.");
      expect((screen.getByTestId("launch-task") as HTMLButtonElement).disabled).toBe(true);
      await user.click(screen.getByTestId("launch-task"));
      expect(created).toHaveLength(0);
    } finally {
      global.fetch = origFetch;
    }
  });

  test("discloses a network-only profile's egress (no connector powers)", async () => {
    localStorage.setItem("engrams:lastProfileId", "pf3");
    const { transport } = installTransport();
    renderWithProviders(<StartScreen />, { transport });
    const user = userEvent.setup();

    expect(await screen.findByText("Egress agent")).toBeTruthy();
    // capCount 0 but reachable > 0 → the receipt still discloses the host.
    await user.click(screen.getByText(/this session can reach/i));
    expect(await screen.findByText("internal.acme.test")).toBeTruthy();
  });
});
