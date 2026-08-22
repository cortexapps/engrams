import { describe, expect, it, vi } from "vitest";
import { fireEvent, screen } from "@testing-library/react";

import { renderWithProviders } from "@/test-utils";
import type { AutomationTestState, TestRenderResult } from "@/hooks/useAutomationTest";

import { SamplePicker, toLocalDateTimeInput } from "./SamplePicker";
import { TestPanel } from "./TestPanel";

const NOW = new Date("2026-08-22T12:00:00Z");

function state(overrides: Partial<AutomationTestState> = {}): AutomationTestState {
  return {
    isTimed: false,
    samples: [
      {
        $typeName: "engram.app.v1.EventSample",
        id: "s1",
        eventKey: "pull_request.opened",
        payloadJson: "{}",
        receivedAt: "2026-08-22T11:50:00Z",
      },
      {
        $typeName: "engram.app.v1.EventSample",
        id: "s2",
        eventKey: "issue_comment.created",
        payloadJson: "{}",
        receivedAt: "2026-08-22T11:00:00Z",
      },
    ],
    samplesLoading: false,
    sample: { kind: "sample", sampleId: "s1" },
    setSample: vi.fn(),
    latest: null,
    tally: null,
    running: false,
    runOnce: vi.fn(async () => null),
    runAcrossSamples: vi.fn(async () => null),
    variableValues: {},
    ...overrides,
  };
}

const RENDERED: TestRenderResult = {
  blocks: [
    {
      blockId: "check",
      blockType: "filter",
      rendered: { conditions: { mode: "all", conditions: [] } },
      filterPass: false,
      scope: { inputs: {} },
    },
    {
      blockId: "launch",
      blockType: "create_session",
      rendered: { profileId: "pr_reviewer", promptTemplate: "Review #41" },
      scope: { inputs: {} },
    },
  ],
  errors: [],
};

const runButton = () => screen.findByRole("button", { name: /^test with sample$/i });

describe("TestPanel", () => {
  it("runs a single render and shows per-block results with filter verdicts", async () => {
    const s = state({ latest: RENDERED });
    renderWithProviders(<TestPanel test={s} now={NOW} />);

    fireEvent.click(await runButton());
    expect(s.runOnce).toHaveBeenCalledTimes(1);

    const check = await screen.findByTestId("test-block-check");
    expect(check.querySelector('[data-verdict="fail"]')).toBeTruthy();
    const launch = screen.getByTestId("test-block-launch");
    expect(launch.querySelector("[data-verdict]")).toBeNull();

    // Expanding a block reveals its rendered config.
    fireEvent.click(screen.getByRole("button", { name: /launch/ }));
    expect(await screen.findByText(/Review #41/)).toBeTruthy();
  });

  it("tallies filter verdicts across recent samples (the 14/20 affordance)", async () => {
    const s = state({
      latest: RENDERED,
      tally: [{ blockId: "check", passed: 14, total: 20 }],
    });
    renderWithProviders(<TestPanel test={s} now={NOW} />);
    fireEvent.click(await screen.findByRole("button", { name: /across 2 samples/i }));
    expect(s.runAcrossSamples).toHaveBeenCalledTimes(1);
    const check = await screen.findByTestId("test-block-check");
    expect(check.querySelector('[data-tally="14/20"]')).toBeTruthy();
  });

  it("surfaces render errors as an inspector pointer", async () => {
    const s = state({
      latest: {
        blocks: [],
        errors: [{ blockId: "launch", field: "promptTemplate", message: "unknown variable" }],
      },
    });
    renderWithProviders(<TestPanel test={s} now={NOW} />);
    expect(await screen.findByText(/1 field did not render/)).toBeTruthy();
  });

  it("disables the run button with no sample; timed triggers can always run and hide across-samples", async () => {
    const noSample = state({ sample: { kind: "none" } });
    const first = renderWithProviders(<TestPanel test={noSample} now={NOW} />);
    expect(((await runButton()) as HTMLButtonElement).disabled).toBe(true);
    first.unmount();

    const timed = state({ isTimed: true, samples: [], sample: { kind: "none" } });
    renderWithProviders(<TestPanel test={timed} now={NOW} />);
    expect(((await runButton()) as HTMLButtonElement).disabled).toBe(false);
    expect(screen.queryByRole("button", { name: /across/i })).toBeNull();
  });
});

describe("SamplePicker", () => {
  it("offers a scheduled-for instant for timed triggers", async () => {
    const onChange = vi.fn();
    renderWithProviders(
      <SamplePicker timed samples={[]} value={{ kind: "none" }} onChange={onChange} now={NOW} />,
    );
    fireEvent.click(await screen.findByRole("button", { name: /now/i }));
    expect(onChange).toHaveBeenCalledWith({ kind: "scheduled", scheduledFor: NOW.toISOString() });
  });

  it("the scheduled-for input shows the instant it stored — no UTC-offset drift on round-trip", async () => {
    // onChange interprets the widget as local wall-clock and stores UTC; the
    // displayed value must convert back, or every edit shifts by the offset.
    // Timezone-independent: whatever TZ the host runs in, local → UTC → local
    // is the identity.
    const local = "2026-08-22T14:00";
    const storedUtc = new Date(local).toISOString();
    expect(toLocalDateTimeInput(storedUtc)).toBe(local);
    expect(toLocalDateTimeInput("not a date")).toBe("");

    renderWithProviders(
      <SamplePicker
        timed
        samples={[]}
        value={{ kind: "scheduled", scheduledFor: storedUtc }}
        onChange={vi.fn()}
        now={NOW}
      />,
    );
    const input = (await screen.findByLabelText("Scheduled for")) as HTMLInputElement;
    expect(input.value).toBe(local);
  });

  it("lists stored samples for event triggers", async () => {
    const s = state();
    renderWithProviders(
      <SamplePicker
        timed={false}
        samples={s.samples}
        value={s.sample}
        onChange={vi.fn()}
        now={NOW}
      />,
    );
    expect(await screen.findByLabelText("Sample")).toBeTruthy();
    // The selected sample renders its event key in the trigger.
    expect(await screen.findByText("pull_request.opened")).toBeTruthy();
  });
});
