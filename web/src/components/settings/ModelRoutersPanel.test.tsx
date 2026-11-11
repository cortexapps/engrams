import { afterEach, describe, expect, test, vi } from "vitest";
import { act, cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { ModelRoutersPanel } from "./ModelRoutersPanel";

const updatePolicy = vi.hoisted(() => vi.fn());
const refresh = vi.hoisted(() => vi.fn());
const putSecret = vi.hoisted(() => vi.fn());
const invalidate = vi.hoisted(() => vi.fn());
const updateSettings = vi.hoisted(() => vi.fn());

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
          egressHosts: ["us.openrouter.ai"],
          connector: "openrouter",
          defaultModel: "",
          titleModel: "",
          decisionModel: "jev",
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
  useUpdateModelRouterSettings: () => ({ mutate: updateSettings, isPending: false, error: null }),
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
  updateSettings.mockClear();
});

describe("ModelRoutersPanel", () => {
  test("shows catalog metadata and enforces the two policy switches", async () => {
    render(<ModelRoutersPanel />);

    expect(screen.getByText("us.openrouter.ai")).toBeTruthy();
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

  test("a role model saves when it changes, and an empty value clears it", async () => {
    render(<ModelRoutersPanel />);

    const title = screen.getByLabelText("Title model");
    await userEvent.type(title, "deepseek/deepseek-v4-pro-0813{Enter}");
    expect(updateSettings).toHaveBeenCalledWith(
      { routerId: "openrouter", titleModel: "deepseek/deepseek-v4-pro-0813" },
      expect.anything(),
    );
    // The typed value stays until the save succeeds.
    expect((title as HTMLInputElement).value).toBe("deepseek/deepseek-v4-pro-0813");
    act(() => {
      updateSettings.mock.calls[0]![1].onSuccess();
      updateSettings.mock.calls[0]![1].onSettled();
    });

    const decision = screen.getByLabelText("Decision model");
    expect((decision as HTMLInputElement).value).toBe("jev");
    await userEvent.click(decision);
    await userEvent.tab();
    expect(updateSettings).toHaveBeenCalledTimes(1);
    await userEvent.clear(decision);
    await userEvent.tab();
    expect(updateSettings).toHaveBeenLastCalledWith(
      { routerId: "openrouter", decisionModel: "" },
      expect.anything(),
    );
  });

  test("a save in flight is not sent twice, and does not hold back another field", async () => {
    render(<ModelRoutersPanel />);

    const title = screen.getByLabelText("Title model");
    await userEvent.type(title, "model-a{Enter}");
    // The blur after Enter sends nothing more; the other field saves on its own.
    const decision = screen.getByLabelText("Decision model");
    await userEvent.clear(decision);
    await userEvent.tab();
    expect(updateSettings.mock.calls.map((call) => call[0])).toEqual([
      { routerId: "openrouter", titleModel: "model-a" },
      { routerId: "openrouter", decisionModel: "" },
    ]);
  });

  test("a failed save keeps the typed value and sends it again on the next blur", async () => {
    render(<ModelRoutersPanel />);

    const title = screen.getByLabelText("Title model") as HTMLInputElement;
    await userEvent.type(title, "model-a{Enter}");
    act(() => updateSettings.mock.calls[0]![1].onSettled());
    expect(title.value).toBe("model-a");
    await userEvent.tab();
    expect(updateSettings).toHaveBeenCalledTimes(2);

    // Escape drops the typed value.
    await userEvent.click(title);
    await userEvent.keyboard("{Escape}");
    expect(title.value).toBe("");
  });

  test("a value typed after a save started stays when that save succeeds", async () => {
    render(<ModelRoutersPanel />);

    const title = screen.getByLabelText("Title model") as HTMLInputElement;
    await userEvent.type(title, "model-a{Enter}");
    await userEvent.type(title, "b");
    act(() => {
      updateSettings.mock.calls[0]![1].onSuccess();
      updateSettings.mock.calls[0]![1].onSettled();
    });
    expect(title.value).toBe("model-ab");
  });
});
