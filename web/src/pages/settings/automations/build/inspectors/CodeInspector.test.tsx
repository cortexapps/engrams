import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";

import { renderWithProviders } from "@/test-utils";
import type { BlockDef } from "@/lib/automation-blocks";
import { create } from "@bufbuild/protobuf";
import { EvalCodeResponseSchema } from "@/gen/engram/app/v1/automation_pb";

import { CodeInspector } from "./CodeInspector";

// The lazy CodeMirror boundary is replaced with a textarea double: what the
// inspector passes (value, readOnly, error line/message) is what we assert.
vi.mock("../CodeEditorLazy", () => ({
  CodeEditorLazy: (props: {
    value: string;
    onChange: (v: string) => void;
    readOnly?: boolean;
    errorLine?: number;
    errorMessage?: string;
  }) => (
    <textarea
      data-testid="code-editor-double"
      value={props.value}
      readOnly={props.readOnly}
      data-error-line={props.errorLine}
      data-error-message={props.errorMessage}
      onChange={(e) => props.onChange(e.target.value)}
    />
  ),
}));

const evalMutate = vi.hoisted(() => vi.fn());
vi.mock("@/hooks/useAutomationCode", async (orig) => ({
  ...(await orig<typeof import("@/hooks/useAutomationCode")>()),
  useEvalCode: () => ({ mutate: evalMutate, isPending: false }),
}));
vi.mock("@/hooks/useAutomationEditor", () => ({
  useEditorAutomation: () => ({
    data: { automation: { inputsJson: JSON.stringify({ mention: "@engrams" }) } },
  }),
}));
vi.mock("@/hooks/useAutomations", () => ({
  useEventSamples: () => ({
    data: {
      samples: [
        {
          id: "s1",
          eventKey: "issues.opened",
          payloadJson: JSON.stringify({ issue: { n: 2 } }),
          receivedAt: "2026-08-22T00:00:00Z",
        },
      ],
    },
  }),
}));
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useParams: () => ({ id: "a1" }),
}));

function block(overrides: Partial<BlockDef> = {}): BlockDef {
  return {
    id: "transform",
    type: "code",
    config: { source: "export default ({ event }) => event.raw.issue.n * 2", mode: "value" },
    ...overrides,
  };
}

function mount(b: BlockDef, builtin = false) {
  const onChange = vi.fn();
  renderWithProviders(
    <CodeInspector
      block={b}
      onChange={onChange}
      builtin={builtin}
      errors={[]}
      sessionSources={[]}
      variablePaths={["event.raw.issue.n"]}
    />,
  );
  return { onChange };
}

function respond(fields: Parameters<typeof create<typeof EvalCodeResponseSchema>>[1]) {
  evalMutate.mockImplementation((_req: unknown, opts?: { onSuccess: (r: unknown) => void }) => {
    opts?.onSuccess(create(EvalCodeResponseSchema, fields));
  });
}

describe("CodeInspector", () => {
  beforeEach(() => evalMutate.mockClear());

  it("runs the source against the latest-sample scope and renders the value", async () => {
    respond({ valueJson: "4", logs: ["hi"], durationMs: 3n });
    mount(block());
    fireEvent.click(await screen.findByTestId("code-run"));

    const [request] = evalMutate.mock.calls[0]!;
    expect(request.mode).toBe("value");
    const input = JSON.parse(request.inputJson);
    expect(input.inputs).toEqual({ mention: "@engrams" });
    expect(input.event.raw).toEqual({ issue: { n: 2 } });
    expect(input.trigger.event).toBe("issues.opened");

    await waitFor(() => expect(screen.getByTestId("code-result").dataset["ok"]).toBe("true"));
    expect(screen.getByTestId("code-result").textContent).toContain("4");
    expect(screen.getByTestId("code-result").textContent).toContain("console (1)");
  });

  it("renders an error with name, message, and line, and squiggles that line", async () => {
    respond({ errorName: "TypeError", errorMessage: "boom", errorLine: 2, durationMs: 1n });
    mount(block());
    fireEvent.click(await screen.findByTestId("code-run"));

    await waitFor(() => expect(screen.getByTestId("code-result").dataset["ok"]).toBe("false"));
    expect(screen.getByTestId("code-result").textContent).toContain("TypeError");
    expect(screen.getByTestId("code-error").textContent).toBe("boom");
    expect(screen.getByTestId("code-result").textContent).toContain("line 2");
    const editor = screen.getByTestId("code-editor-double");
    expect(editor.dataset["errorLine"]).toBe("2");
    expect(editor.dataset["errorMessage"]).toBe("TypeError: boom");
  });

  it("is read-only when the source is pinned on a built-in", async () => {
    mount(block(), true);
    expect(await screen.findByTestId("code-editor-double")).toHaveProperty("readOnly", true);
    // Both source and mode are pinned on this block.
    expect(screen.getAllByText("Set by the built-in").length).toBeGreaterThan(0);
  });

  it("is editable when the source is tunable on a built-in", async () => {
    mount(block({ tunable: ["source"] }), true);
    expect(await screen.findByTestId("code-editor-double")).toHaveProperty("readOnly", false);
  });

  it("edits flow back through onChange", async () => {
    const { onChange } = mount(block());
    fireEvent.change(await screen.findByTestId("code-editor-double"), {
      target: { value: "export default () => true" },
    });
    expect(onChange).toHaveBeenCalledWith(
      expect.objectContaining({
        config: expect.objectContaining({ source: "export default () => true" }),
      }),
    );
  });

  it("never silently succeeds on an empty response", async () => {
    respond({ durationMs: 0n });
    mount(block());
    fireEvent.click(await screen.findByTestId("code-run"));
    await waitFor(() => expect(screen.getByTestId("code-result").dataset["ok"]).toBe("false"));
    expect(screen.getByTestId("code-result").textContent).toContain("ContractError");
  });
});
