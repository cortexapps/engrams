import { render, screen } from "@testing-library/react";
import { describe, expect, test } from "vitest";

import { SpecBlockTranscriptChip } from "./SpecBlockTranscriptChip";

describe("SpecBlockTranscriptChip", () => {
  test("shows the checkpointed source update in the main transcript", () => {
    render(
      <SpecBlockTranscriptChip
        type="tool-call"
        toolName="engram.specBlockUpdate"
        toolCallId="tool-1"
        args={{
          section_id: "design",
          block_id: "request-flow",
          source: "flowchart LR\nA --> Retry",
        }}
        argsText=""
        result={{ applied: true, checkpoint_id: "00000000-0000-4000-8000-000000001124" }}
        status={{ type: "complete" }}
        addResult={() => {}}
        resume={() => {}}
        respondToApproval={() => {}}
      />,
    );

    expect(screen.getByTestId("spec-block-transcript-chip").textContent).toContain(
      "Updated block request-flow",
    );
    expect(screen.getByText("checkpoint 00000000")).toBeTruthy();
    expect(
      screen.getByText("Source change").closest("details")?.querySelector("pre")?.textContent,
    ).toBe("flowchart LR\nA --> Retry");
    expect(screen.getByLabelText("Block update checkpointed")).toBeTruthy();
  });

  test("shows a rejected source update as incomplete", () => {
    render(
      <SpecBlockTranscriptChip
        type="tool-call"
        toolName="engram.specBlockUpdate"
        toolCallId="tool-2"
        args={{ section_id: "design", block_id: "request-flow", source: "new" }}
        argsText=""
        result={{ applied: false }}
        status={{ type: "complete" }}
        addResult={() => {}}
        resume={() => {}}
        respondToApproval={() => {}}
      />,
    );

    expect(screen.getByText("Could not update block request-flow")).toBeTruthy();
    expect(screen.getByLabelText("Block update failed")).toBeTruthy();
  });

  test("limits a large source preview", () => {
    render(
      <SpecBlockTranscriptChip
        type="tool-call"
        toolName="engram.specBlockUpdate"
        toolCallId="tool-3"
        args={{ section_id: "design", block_id: "request-flow", source: "x".repeat(25_000) }}
        argsText=""
        result={{ applied: true, checkpoint_id: "00000000-0000-4000-8000-000000001124" }}
        status={{ type: "complete" }}
        addResult={() => {}}
        resume={() => {}}
        respondToApproval={() => {}}
      />,
    );

    expect(screen.getByText("Source change (preview)")).toBeTruthy();
    expect(screen.getByText("The source preview is limited.")).toBeTruthy();
    const preview = screen
      .getByText("Source change (preview)")
      .closest("details")
      ?.querySelector("pre")?.textContent;
    expect(preview).toBe("x".repeat(20_000));
  });
});
