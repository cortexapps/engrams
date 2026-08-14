import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { ModelRoutersPanel } from "./ModelRoutersPanel";

const updatePolicy = vi.hoisted(() => vi.fn());
const refresh = vi.hoisted(() => vi.fn());
const putSecret = vi.hoisted(() => vi.fn());
const invalidate = vi.hoisted(() => vi.fn());

vi.mock("@/hooks/useModelRouters", () => ({
  useModelRouters: () => ({
    data: {
      routers: [
        {
          id: "openrouter",
          label: "OpenRouter",
          description: "Shared routed models.",
          protocols: ["anthropic_messages", "openai_responses"],
          credentialSecret: "openrouter.api_key",
          credentialConfigured: true,
          availableModelCount: 270,
          enabledModelCount: 3,
          modelCount: 270,
        },
      ],
    },
    isLoading: false,
    error: null,
  }),
  useRouterModels: () => ({
    data: {
      models: [
        {
          id: "deepseek/deepseek-v4-pro-0813",
          name: "DeepSeek V4 Pro 0813",
          author: "deepseek",
          available: true,
          enabled: true,
          userEnabled: true,
          supportsReasoning: true,
          inputModalities: ["text"],
          contextLength: 163_840n,
          promptPrice: "0.0000002",
          completionPrice: "0.0000008",
          upstreamUrl: "https://openrouter.ai/deepseek/deepseek-v4-pro-0813",
        },
      ],
    },
    isLoading: false,
    error: null,
  }),
  useRefreshRouterModels: () => ({ mutate: refresh, isPending: false, error: null }),
  useUpdateRouterModelPolicy: () => ({ mutate: updatePolicy, isPending: false }),
  useInvalidateRouter: () => invalidate,
}));

vi.mock("@/hooks/useOrgSecrets", () => ({
  usePutOrgSecret: () => ({ mutateAsync: putSecret, isPending: false, error: null }),
}));

afterEach(() => {
  cleanup();
  updatePolicy.mockClear();
  refresh.mockClear();
  putSecret.mockClear();
  invalidate.mockClear();
});

describe("ModelRoutersPanel", () => {
  test("shows catalog metadata and enforces the two policy switches", async () => {
    render(<ModelRoutersPanel />);

    expect(screen.getByText("270 available")).toBeTruthy();
    expect(screen.getByText("DeepSeek V4 Pro 0813")).toBeTruthy();
    expect(screen.getByText("deepseek")).toBeTruthy();
    expect(screen.getByText("reasoning")).toBeTruthy();

    await userEvent.click(screen.getByLabelText("Enable DeepSeek V4 Pro 0813"));
    expect(updatePolicy).toHaveBeenCalledWith({
      routerId: "openrouter",
      modelId: "deepseek/deepseek-v4-pro-0813",
      enabled: false,
      userEnabled: false,
    });

    await userEvent.click(screen.getByRole("button", { name: "Refresh catalog" }));
    expect(refresh).toHaveBeenCalledWith({ routerId: "openrouter" });
  });
});
