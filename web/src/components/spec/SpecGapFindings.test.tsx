import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";

import type { SpecGapFinding } from "@/hooks/useSpecGapCheck";

import { SpecGapFindings } from "./SpecGapFindings";

function finding(overrides: Partial<SpecGapFinding> & Pick<SpecGapFinding, "id">): SpecGapFinding {
  return {
    kind: "requirement_gap",
    severity: "gap",
    layerKey: "system",
    sectionId: "sec-api",
    sectionTitle: "API surface",
    requirementId: "R3",
    summary: "R3 has no Design coverage",
    detail: "R3 has no API shape — reset_at is missing from the 429 payload in §API surface.",
    proposedDiff: null,
    disposition: "pending",
    openQuestionId: null,
    disposedAt: null,
    ...overrides,
  };
}

describe("SpecGapFindings", () => {
  test("offers an open question at the finding's own anchor", async () => {
    const user = userEvent.setup();
    const onDispose = vi.fn();
    render(
      <SpecGapFindings
        findings={[finding({ id: "f1" })]}
        stoppedAtLayerKey={null}
        suppressedCount={0}
        editable
        onDispose={onDispose}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Open question @ §API surface" }));

    expect(onDispose).toHaveBeenCalledWith("f1", "open_question");
  });

  test("offers the proposed diff only when the finding carries one", async () => {
    const user = userEvent.setup();
    const onDispose = vi.fn();
    render(
      <SpecGapFindings
        findings={[
          finding({ id: "f1" }),
          finding({
            id: "f2",
            proposedDiff: { sectionId: "sec-api", before: "old", after: "new" },
          }),
        ]}
        stoppedAtLayerKey={null}
        suppressedCount={0}
        editable
        onDispose={onDispose}
      />,
    );

    const accept = screen.getAllByRole("button", { name: "Accept proposed diff" });
    expect(accept).toHaveLength(1);

    await user.click(accept[0]!);
    expect(onDispose).toHaveBeenCalledWith("f2", "accept_diff");
  });

  test("labels the finding that stopped the pass and counts the withheld ones", () => {
    render(
      <SpecGapFindings
        findings={[
          finding({
            id: "f-fatal",
            kind: "red_team",
            severity: "fatal",
            layerKey: "contract",
            sectionTitle: "Failure modes",
            detail: "Failure modes assume a 30s cache TTL, but Behavior promises a sharper reset.",
          }),
        ]}
        stoppedAtLayerKey="contract"
        suppressedCount={3}
        editable
        onDispose={vi.fn()}
      />,
    );

    expect(screen.getByText("red-team · stopped outside-in")).toBeTruthy();
    expect(screen.getByText(/3 findings below this layer are withheld/)).toBeTruthy();
  });

  test("a disposed finding shows its outcome and offers no more actions", () => {
    render(
      <SpecGapFindings
        findings={[finding({ id: "f1", disposition: "question_opened" })]}
        stoppedAtLayerKey={null}
        suppressedCount={0}
        editable
        onDispose={vi.fn()}
      />,
    );

    expect(screen.getByText("question opened")).toBeTruthy();
    expect(screen.queryByRole("button")).toBeNull();
  });

  test("a read-only spec offers no dispositions at all", () => {
    render(
      <SpecGapFindings
        findings={[finding({ id: "f1" })]}
        stoppedAtLayerKey={null}
        suppressedCount={0}
        editable={false}
        onDispose={vi.fn()}
      />,
    );

    expect(screen.queryByRole("button")).toBeNull();
  });

  test("a clean pass says so", () => {
    render(
      <SpecGapFindings
        findings={[]}
        stoppedAtLayerKey={null}
        suppressedCount={0}
        editable
        onDispose={vi.fn()}
      />,
    );

    expect(screen.getByText("This pass found nothing to report.")).toBeTruthy();
  });
});
