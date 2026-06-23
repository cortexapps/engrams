// Contract test for the full-page Launch picker (redesign §H): renders the
// profile cards + the "this session will be able to" receipt, and launching
// calls CreateTask with the selected profile + prompt.

import { afterEach, describe, expect, test } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../test-utils";
import { LaunchPage } from "./LaunchPage";
import { TaskService } from "../gen/engram/app/v1/task_pb";
import { ProfileService } from "../gen/engram/app/v1/profile_pb";
import { IntegrationService } from "../gen/engram/app/v1/integration_pb";
import { ImageService } from "../gen/engram/app/v1/image_pb";

const PROFILE = {
  id: "pf1",
  name: "Bug-fix agent",
  description: "Reproduce, fix, open a PR.",
  icon: "Bot",
  imageId: "img1",
  includeUserTokens: false,
  envVars: {},
  skills: [],
  capabilities: ["github:pulls:write"],
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

function installTransport() {
  const created: Array<{ type: string; profileId: string; prompt?: string }> = [];
  const transport = createRouterTransport((router) => {
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
      listProfiles: () => ({ profiles: [PROFILE] }),
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

describe("LaunchPage", () => {
  afterEach(() => cleanup());

  test("renders the profile card + receipt and launches with the prompt", async () => {
    const { transport, created } = installTransport();
    renderWithProviders(<LaunchPage />, { transport });
    const user = userEvent.setup();

    expect(await screen.findByText("Bug-fix agent")).toBeTruthy();
    // The default-selected profile's receipt derives a write power from the catalog.
    expect(await screen.findByText(/this session will be able to/i)).toBeTruthy();
    expect(screen.getByText(/write pulls/i)).toBeTruthy();
    // api.github.com is opened by the granted power.
    expect(screen.getByText("api.github.com")).toBeTruthy();

    await user.type(screen.getByLabelText("Task"), "Fix the flaky test and open a PR.");
    await user.click(screen.getByTestId("launch-session"));

    await waitFor(() => expect(created).toHaveLength(1));
    expect(created[0]).toEqual({
      type: "chat",
      profileId: "pf1",
      prompt: "Fix the flaky test and open a PR.",
    });
  });
});
