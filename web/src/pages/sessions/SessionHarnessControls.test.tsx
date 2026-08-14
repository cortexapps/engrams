import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { create } from "@bufbuild/protobuf";
import { HarnessSummarySchema } from "../../gen/engram/app/v1/harness_pb";
import { ModeChip } from "../../components/ModeChip";
import { EMPTY_OVERRIDE, SessionHarnessControls } from "./SessionHarnessControls";

const routerCatalog = vi.hoisted(() => ({
  routers: [] as Array<{
    id: string;
    label: string;
    protocols: string[];
    defaultModel: string;
  }>,
  models: [] as Array<{
    id: string;
    name: string;
    supportsReasoning: boolean;
  }>,
}));
vi.mock("@/hooks/useModelRouters", () => ({
  useModelRouters: () => ({ data: { routers: routerCatalog.routers } }),
  useRouterModels: () => ({ data: { models: routerCatalog.models } }),
}));

afterEach(() => {
  cleanup();
  routerCatalog.routers = [];
  routerCatalog.models = [];
});

const codex = create(HarnessSummarySchema, {
  name: "codex",
  descriptor: {
    name: "codex",
    label: "Codex",
    models: [
      { id: "gpt-5", label: "GPT-5", default: true },
      { id: "gpt-5-mini", label: "GPT-5 mini" },
    ],
    effort: [
      { id: "medium", label: "Medium", default: true },
      { id: "high", label: "High" },
    ],
    modes: [{ id: "plan", label: "Plan" }],
  },
});

describe("SessionHarnessControls", () => {
  // The row must state what the launch will actually use. "Default model" told
  // the user nothing and cost a whole pill's width.
  test("shows the descriptor default, not the word Default", () => {
    render(
      <SessionHarnessControls
        harnesses={[codex]}
        profileHarness="codex"
        value={EMPTY_OVERRIDE}
        onChange={() => {}}
      />,
    );
    expect(screen.getByTestId("session-model-select").textContent).toContain("GPT-5");
    expect(screen.getByTestId("session-effort-select").textContent).toContain("Medium");
    // One harness = nothing to choose.
    expect(screen.queryByTestId("session-harness-select")).toBeNull();
  });

  test("picking an option records the override", async () => {
    const onChange = vi.fn();
    render(
      <SessionHarnessControls
        harnesses={[codex]}
        profileHarness="codex"
        value={EMPTY_OVERRIDE}
        onChange={onChange}
      />,
    );
    await userEvent.click(screen.getByTestId("session-effort-select"));
    await userEvent.click(await screen.findByRole("menuitem", { name: "High" }));
    expect(onChange).toHaveBeenCalledWith({ ...EMPTY_OVERRIDE, effort: "high" });
  });

  test("changing the harness clears its stale model/effort but keeps the mode", async () => {
    const claude = create(HarnessSummarySchema, {
      name: "claude",
      descriptor: { name: "claude", label: "Claude Code" },
    });
    const onChange = vi.fn();
    render(
      <SessionHarnessControls
        harnesses={[codex, claude]}
        profileHarness="codex"
        value={{ harness: null, model: "gpt-5-mini", effort: "high", mode: "plan" }}
        onChange={onChange}
      />,
    );
    await userEvent.click(screen.getByTestId("session-harness-select"));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Claude Code" }));
    expect(onChange).toHaveBeenCalledWith({
      harness: "claude",
      modelRouter: null,
      model: null,
      effort: null,
      mode: "plan",
    });
  });

  test("changing to a compatible harness preserves an explicit routed model", async () => {
    routerCatalog.routers = [
      {
        id: "openrouter",
        label: "OpenRouter",
        protocols: ["anthropic_messages", "openai_responses"],
        defaultModel: "deepseek/deepseek-v4-pro-0813",
      },
    ];
    routerCatalog.models = [
      {
        id: "deepseek/deepseek-v4-pro-0813",
        name: "DeepSeek V4 Pro 0813",
        supportsReasoning: true,
      },
    ];
    const claude = create(HarnessSummarySchema, {
      name: "claude",
      descriptor: {
        name: "claude",
        label: "Claude Code",
        routerProtocols: ["anthropic_messages"],
      },
    });
    const routedCodex = create(HarnessSummarySchema, {
      name: "codex",
      descriptor: {
        name: "codex",
        label: "Codex",
        routerProtocols: ["openai_responses"],
        models: [
          { id: "gpt-5", label: "GPT-5", default: true },
          { id: "gpt-5-mini", label: "GPT-5 mini" },
        ],
        effort: [
          { id: "medium", label: "Medium", default: true },
          { id: "high", label: "High" },
        ],
        modes: [{ id: "plan", label: "Plan" }],
      },
    });
    const onChange = vi.fn();
    render(
      <SessionHarnessControls
        harnesses={[routedCodex, claude]}
        profileHarness="codex"
        value={{
          harness: null,
          modelRouter: "openrouter",
          model: "deepseek/deepseek-v4-pro-0813",
          effort: "high",
          mode: "plan",
        }}
        onChange={onChange}
      />,
    );

    expect(screen.getByTestId("session-model-select").textContent).toContain(
      "DeepSeek V4 Pro 0813",
    );
    await userEvent.click(screen.getByTestId("session-harness-select"));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Claude Code" }));
    expect(onChange).toHaveBeenCalledWith({
      harness: "claude",
      modelRouter: "openrouter",
      model: "deepseek/deepseek-v4-pro-0813",
      effort: "high",
      mode: "plan",
    });
  });
});

describe("ModeChip", () => {
  test("a single alternate mode is a toggle, both ways", async () => {
    const onChange = vi.fn();
    const { rerender } = render(
      <ModeChip modes={[{ id: "plan", label: "Plan" }]} value={null} onChange={onChange} />,
    );
    const chip = screen.getByTestId("session-mode-chip");
    expect(chip.getAttribute("aria-pressed")).toBe("false");
    await userEvent.click(chip);
    expect(onChange).toHaveBeenCalledWith("plan");

    rerender(<ModeChip modes={[{ id: "plan", label: "Plan" }]} value="plan" onChange={onChange} />);
    expect(screen.getByTestId("session-mode-chip").getAttribute("aria-pressed")).toBe("true");
    await userEvent.click(screen.getByTestId("session-mode-chip"));
    expect(onChange).toHaveBeenLastCalledWith(null);
  });

  test("several modes become a menu that can return to the default", async () => {
    const onChange = vi.fn();
    render(
      <ModeChip
        modes={[
          { id: "plan", label: "Plan" },
          { id: "review", label: "Review" },
        ]}
        value="plan"
        onChange={onChange}
      />,
    );
    expect(screen.getByTestId("session-mode-chip").textContent).toContain("Plan");
    await userEvent.click(screen.getByTestId("session-mode-chip"));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Build" }));
    expect(onChange).toHaveBeenCalledWith(null);
  });

  test("a harness with no declared modes renders nothing", () => {
    const { container } = render(<ModeChip modes={[]} value={null} onChange={() => {}} />);
    expect(container.innerHTML).toBe("");
  });
});
