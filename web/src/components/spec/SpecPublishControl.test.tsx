import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, test, vi } from "vitest";

import { SpecRequestError } from "@/lib/spec-api";
import type { SpecPublishStatus } from "@/hooks/useSpecPublish";

import { SpecPublishControl } from "./SpecPublishControl";

const state: { status: SpecPublishStatus | null; error: unknown } = { status: null, error: null };
const mutate = vi.fn(
  (
    input: { acknowledgeOpenQuestions: boolean; runGapCheck: boolean },
    options?: { onError?: (error: unknown) => void },
  ) => {
    void input;
    if (state.error) options?.onError?.(state.error);
  },
);

vi.mock("@/hooks/useSpecPublish", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/hooks/useSpecPublish")>();
  return {
    ...original,
    useSpecPublish: () => ({ data: state.status }),
    usePublishSpec: () => ({ mutate, isPending: false }),
  };
});

function status(overrides: Partial<SpecPublishStatus> = {}): SpecPublishStatus {
  return {
    lifecycle: "draft",
    canPublish: true,
    publishedAt: null,
    gate: {
      ready: true,
      settledRequiredCount: 9,
      requiredCount: 9,
      acknowledgmentRequired: false,
      gapCheckRunRequired: false,
      blockers: [],
      openQuestions: [],
    },
    gapCheck: {
      stale: false,
      runId: "run-1",
      ranAt: "2026-08-12T14:31:00.000Z",
      gates: true,
    },
    publish: null,
    ...overrides,
  };
}

function blocked(): SpecPublishStatus {
  return status({
    gate: {
      ready: false,
      settledRequiredCount: 7,
      requiredCount: 9,
      acknowledgmentRequired: false,
      gapCheckRunRequired: false,
      blockers: [
        {
          sectionId: "sec-data",
          sectionTitle: "Data model",
          layerKey: "contract",
          state: "drafted",
          reason: "drafted",
        },
        {
          sectionId: "sec-api",
          sectionTitle: "API surface",
          layerKey: "contract",
          state: "empty",
          reason: "empty",
        },
      ],
      openQuestions: [],
    },
  });
}

function withQuestions(): SpecPublishStatus {
  return status({
    gate: {
      ready: true,
      settledRequiredCount: 9,
      requiredCount: 9,
      acknowledgmentRequired: true,
      gapCheckRunRequired: false,
      blockers: [],
      openQuestions: [
        {
          id: "q-4",
          sectionId: "sec-data",
          sectionTitle: "Data model",
          text: "Do banked burst credits survive a plan downgrade?",
        },
        {
          id: "q-5",
          sectionId: "sec-failure",
          sectionTitle: "Failure modes",
          text: "Fail closed on Postgres loss — who signs off on revenue over availability?",
        },
        {
          id: "q-6",
          sectionId: "sec-api",
          sectionTitle: "API surface",
          text: "Does the 429 payload expose remaining quota, or just the ceiling?",
        },
      ],
    },
  });
}

beforeEach(() => {
  mutate.mockClear();
  state.status = status();
  state.error = null;
});

describe("SpecPublishControl", () => {
  test("the button is quiet while the gate blocks and filled when it passes (R34)", () => {
    state.status = blocked();
    const { rerender } = render(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);

    const quiet = screen.getByRole("button", { name: /Publish — 2 required sections/ });
    expect(quiet.className).not.toContain("bg-primary");

    state.status = status();
    rerender(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);
    const filled = screen.getByRole("button", { name: /Publish — the gate passes/ });
    expect(filled.className).toContain("bg-primary");
  });

  test("the blocked dialog lists every blocker one click from its section", async () => {
    const user = userEvent.setup();
    const onReviewSection = vi.fn();
    state.status = blocked();
    render(<SpecPublishControl specId="spec-1" onReviewSection={onReviewSection} />);

    await user.click(screen.getByRole("button", { name: /Publish/ }));

    expect(screen.getByText("2 required sections are not settled")).toBeTruthy();
    expect(screen.getByText("Data model")).toBeTruthy();
    expect(screen.getByText("— drafted, not confirmed")).toBeTruthy();
    expect(screen.getByText("API surface")).toBeTruthy();
    expect(screen.getByText("— empty")).toBeTruthy();
    // The ran-at hour follows the viewer's timezone, so the assertion must not
    // pin one (a UTC-authored literal fails on any laptop west of Greenwich).
    expect(screen.getByText(/^7 of 9 ready · gap check ran \d{1,2}:31\s?(AM|PM)$/)).toBeTruthy();

    const reviews = screen.getAllByRole("button", { name: "Review →" });
    await user.click(reviews[0]!);
    expect(onReviewSection).toHaveBeenCalledWith("sec-data");
  });

  test("open questions are written out in full and the count is in the checkbox (R35)", async () => {
    const user = userEvent.setup();
    state.status = withQuestions();
    render(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);

    await user.click(screen.getByRole("button", { name: /Publish/ }));

    expect(screen.getByText("3 questions go into the tickets unanswered")).toBeTruthy();
    expect(screen.getByText("Do banked burst credits survive a plan downgrade?")).toBeTruthy();
    expect(
      screen.getByText(
        "Fail closed on Postgres loss — who signs off on revenue over availability?",
      ),
    ).toBeTruthy();
    expect(
      screen.getByText("Does the 429 payload expose remaining quota, or just the ceiling?"),
    ).toBeTruthy();
    expect(screen.getByText("3 open questions")).toBeTruthy();
    expect(screen.getByText("§Data model")).toBeTruthy();
    // One-way, stated in the footer without alarm colour.
    expect(screen.getByText(/one-way: ends drafting, spec stays readable/)).toBeTruthy();
  });

  test("publishing with open questions waits for the acknowledgment (R35)", async () => {
    const user = userEvent.setup();
    state.status = withQuestions();
    render(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);
    await user.click(screen.getByRole("button", { name: /Publish/ }));

    const primary = screen.getByRole<HTMLButtonElement>("button", {
      name: "Publish & ticketize",
    });
    expect(primary.disabled).toBe(true);

    await user.click(screen.getByRole("checkbox"));
    expect(primary.disabled).toBe(false);

    await user.click(primary);
    expect(mutate).toHaveBeenCalledWith(
      { acknowledgeOpenQuestions: true, runGapCheck: false },
      expect.anything(),
    );
  });

  test("a stale gap check makes the run part of the publish (R30)", async () => {
    const user = userEvent.setup();
    state.status = status({
      gate: { ...status().gate, gapCheckRunRequired: true },
      gapCheck: { stale: true, runId: null, ranAt: null, gates: true },
    });
    render(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);

    await user.click(screen.getByRole("button", { name: /Run gap check & publish/ }));
    expect(screen.getByText("9 of 9 ready · gap check has not run")).toBeTruthy();

    const dialog = within(screen.getByRole("dialog"));
    await user.click(dialog.getByRole("button", { name: "Run gap check & publish" }));
    expect(mutate).toHaveBeenCalledWith(
      { acknowledgeOpenQuestions: false, runGapCheck: true },
      expect.anything(),
    );
  });

  test("a refusal replaces the gate in the open dialog, and it stays open", async () => {
    const user = userEvent.setup();
    state.status = status();
    state.error = new SpecRequestError("/specs/spec-1/publish", 409, "2 sections", {
      message: "2 required sections are not settled.",
      reason: "blocked",
      status: blocked(),
    });
    render(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);

    await user.click(screen.getByRole("button", { name: /Publish/ }));
    await user.click(screen.getByRole("button", { name: "Publish & ticketize" }));

    expect(screen.getByText("2 required sections are not settled")).toBeTruthy();
    expect(screen.getAllByRole("button", { name: "Review →" })).toHaveLength(2);
  });

  test("a recorded publish shows the steps, and the gate is gone", async () => {
    const user = userEvent.setup();
    state.status = status({
      canPublish: false,
      publish: {
        state: "pinned",
        checkpointId: "cp-1",
        artifactId: "art-1",
        artifactVersion: null,
        acknowledgedQuestionCount: 3,
        requestedAt: "2026-08-12T15:04:00.000Z",
        pinnedAt: "2026-08-12T15:04:01.000Z",
        completedAt: null,
        lastError: null,
      },
    });
    render(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);

    await user.click(screen.getByRole("button", { name: /Publishing/ }));

    expect(screen.getByText("This spec is published")).toBeTruthy();
    expect(screen.getByText("Pinned the checkpoint")).toBeTruthy();
    expect(screen.getByText("Writing the shared version")).toBeTruthy();
    expect(screen.getByText("3 open questions were carried into the tickets.")).toBeTruthy();
  });

  test("a refused pin re-opens the gate and says nothing was published", async () => {
    const user = userEvent.setup();
    state.status = {
      ...blocked(),
      canPublish: true,
      publish: {
        state: "blocked",
        checkpointId: "cp-1",
        artifactId: "art-1",
        artifactVersion: null,
        acknowledgedQuestionCount: 0,
        requestedAt: "2026-08-12T15:04:00.000Z",
        pinnedAt: null,
        completedAt: null,
        lastError: "1 required sections are no longer settled.",
      },
    };
    render(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);

    // The gate is what the person needs next, not a progress dialog.
    await user.click(screen.getByRole("button", { name: /Publish — 2 required sections/ }));

    expect(screen.getByText("2 required sections are not settled")).toBeTruthy();
    expect(
      screen.getByText(
        /The last publish did not pin: 1 required sections are no longer settled\. Nothing was published\./,
      ),
    ).toBeTruthy();
    expect(screen.getAllByRole("button", { name: "Review →" })).toHaveLength(2);
  });

  test("a finished publish offers no control at all", () => {
    state.status = status({
      canPublish: false,
      publish: {
        state: "complete",
        checkpointId: "cp-1",
        artifactId: "art-1",
        artifactVersion: 1,
        acknowledgedQuestionCount: 0,
        requestedAt: "2026-08-12T15:04:00.000Z",
        pinnedAt: "2026-08-12T15:04:01.000Z",
        completedAt: "2026-08-12T15:04:03.000Z",
        lastError: null,
      },
    });
    render(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);

    expect(screen.queryByRole("button", { name: /Publish/ })).toBeNull();
  });

  test("a running publish stays reachable, and it reports its retry", async () => {
    const user = userEvent.setup();
    state.status = status({
      canPublish: false,
      publish: {
        state: "pinned",
        checkpointId: "cp-1",
        artifactId: "art-1",
        artifactVersion: null,
        acknowledgedQuestionCount: 0,
        requestedAt: "2026-08-12T15:04:00.000Z",
        pinnedAt: "2026-08-12T15:04:01.000Z",
        completedAt: null,
        lastError: "the sandbox is not reachable",
      },
    });
    render(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);

    await user.click(screen.getByRole("button", { name: /Publishing/ }));

    expect(
      screen.getByText(
        "The last step did not finish: the sandbox is not reachable. It retries on its own.",
      ),
    ).toBeTruthy();
  });

  test("a non-owner member sees no publish button (R37)", () => {
    state.status = status({ canPublish: false });
    render(<SpecPublishControl specId="spec-1" onReviewSection={vi.fn()} />);

    expect(screen.queryByRole("button", { name: /Publish/ })).toBeNull();
  });
});
