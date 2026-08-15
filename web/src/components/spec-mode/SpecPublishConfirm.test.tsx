import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, test, vi } from "vitest";

import type { PublishSpecInput, SpecPublishStatus } from "@/hooks/useSpecPublish";
import { SpecRequestError } from "@/lib/spec-api";

const state: { status: SpecPublishStatus; error: unknown } = {
  status: {
    phase: "drafting",
    canPublish: true,
    openQuestions: [],
    publish: null,
  },
  error: null,
};

const mutate = vi.fn(
  (
    _input: PublishSpecInput,
    options?: {
      onSuccess?: () => void;
      onError?: (error: unknown) => void;
    },
  ) => {
    if (state.error) options?.onError?.(state.error);
    else options?.onSuccess?.();
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

// The readiness line reads the rail; these tests exercise the questions face.
vi.mock("@/hooks/useSpecRead", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/hooks/useSpecRead")>();
  return {
    ...original,
    useSpecRail: () => ({ data: undefined }),
  };
});

import { SpecPublishConfirm } from "./SpecPublishConfirm";

const questions = [
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
    text: "Who signs off on revenue over availability?",
  },
  {
    id: "q-6",
    sectionId: "sec-api",
    sectionTitle: "API surface",
    text: "Does the 429 payload expose remaining quota?",
  },
];

beforeEach(() => {
  state.status = {
    phase: "drafting",
    canPublish: true,
    openQuestions: [],
    publish: null,
  };
  state.error = null;
  mutate.mockClear();
});

describe("SpecPublishConfirm", () => {
  test("states the open-question count, names the questions, and says publishing is one way", async () => {
    const user = userEvent.setup();
    state.status = { ...state.status, openQuestions: questions };
    render(<SpecPublishConfirm specId="spec-1" viewerIsOwner />);

    const trigger = screen.getByRole<HTMLButtonElement>("button", { name: "Publish" });
    expect(trigger.disabled).toBe(false);
    await user.click(trigger);

    expect(screen.getByText("Publish with 3 open questions?")).toBeTruthy();
    expect(
      screen.getByText("3 open questions will remain unresolved. They do not block publication."),
    ).toBeTruthy();
    expect(screen.getByText("Do banked burst credits survive a plan downgrade?")).toBeTruthy();
    expect(screen.getByText("Who signs off on revenue over availability?")).toBeTruthy();
    expect(screen.getByText("Does the 429 payload expose remaining quota?")).toBeTruthy();
    expect(screen.getByText("§Data model")).toBeTruthy();
    expect(
      screen.getByText(
        "Publishing is irreversible. It is a one-way action: drafting ends, and this spec becomes read-only.",
      ),
    ).toBeTruthy();
  });

  test("requires acknowledgment only when open questions exist", async () => {
    const user = userEvent.setup();
    state.status = { ...state.status, openQuestions: questions };
    const view = render(<SpecPublishConfirm specId="spec-1" viewerIsOwner />);

    await user.click(screen.getByRole("button", { name: "Publish" }));
    const dialogPublish = screen.getAllByRole("button", { name: "Publish" }).at(-1)!;
    expect((dialogPublish as HTMLButtonElement).disabled).toBe(false);

    await user.click(dialogPublish);
    expect(mutate).not.toHaveBeenCalled();
    expect(screen.getByRole("alert").textContent).toContain(
      "Acknowledge the 3 open questions before you publish.",
    );

    await user.click(screen.getByRole("checkbox"));
    await user.click(dialogPublish);
    expect(mutate).toHaveBeenCalledWith({ acknowledgeOpenQuestions: true }, expect.anything());

    view.unmount();
    state.status = { ...state.status, openQuestions: [] };
    render(<SpecPublishConfirm specId="spec-1" viewerIsOwner />);
    await user.click(screen.getByRole("button", { name: "Publish" }));

    expect(screen.queryByRole("checkbox")).toBeNull();
    expect(screen.getByText("This spec has 0 open questions.")).toBeTruthy();
    await user.click(screen.getAllByRole("button", { name: "Publish" }).at(-1)!);
    expect(mutate).toHaveBeenLastCalledWith({ acknowledgeOpenQuestions: false }, expect.anything());
  });

  test("shows a readable refusal when publishing during ideation", async () => {
    const user = userEvent.setup();
    state.status = {
      phase: "ideation",
      canPublish: false,
      openQuestions: [],
      publish: null,
    };
    state.error = new SpecRequestError("/specs/spec-1/publish", 409, "internal phase mismatch", {
      reason: "ideation",
      message: "internal phase mismatch",
      status: state.status,
    });
    render(<SpecPublishConfirm specId="spec-1" viewerIsOwner />);

    await user.click(screen.getByRole("button", { name: "Publish" }));
    await user.click(screen.getAllByRole("button", { name: "Publish" }).at(-1)!);

    expect(screen.getByText("Start drafting before you publish this spec.")).toBeTruthy();
    expect(screen.queryByText("internal phase mismatch")).toBeNull();
  });

  test("does not render a publish trigger for a non-owner", () => {
    render(<SpecPublishConfirm specId="spec-1" viewerIsOwner={false} />);

    expect(screen.queryByRole("button", { name: "Publish" })).toBeNull();
  });

  test("keeps the running publish time assertion independent of the viewer timezone", async () => {
    const user = userEvent.setup();
    state.status = {
      phase: "drafting",
      canPublish: false,
      openQuestions: [],
      publish: {
        state: "pinned",
        checkpointId: "cp-1",
        artifactId: "art-1",
        artifactVersion: null,
        acknowledgedQuestionCount: 0,
        requestedAt: "2026-08-12T14:31:00.000Z",
        pinnedAt: "2026-08-12T14:31:01.000Z",
        completedAt: null,
        lastError: null,
      },
    };
    render(<SpecPublishConfirm specId="spec-1" viewerIsOwner />);

    await user.click(screen.getByRole("button", { name: "Publishing…" }));

    // The hour follows the viewer's timezone, so the assertion must not pin one
    // (a UTC-authored literal fails on any laptop west of Greenwich).
    expect(
      screen.getByText(
        /^The request started at \d{1,2}:31\s?(AM|PM)\. Publishing continues if you close this window\.$/,
      ),
    ).toBeTruthy();
  });
});
