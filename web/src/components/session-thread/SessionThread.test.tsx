// Smoke test: mount SessionThread through the external-store runtime and
// confirm the SSE-derived messages actually reach the DOM. We feed a
// completed run (non-busy) so no loop animation / Web-Animations path mounts
// under jsdom.

import { afterEach, describe, expect, test } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { SessionService } from "../../gen/engram/app/v1/session_pb";
import { SessionThread } from "./SessionThread";
import type { IndexedEvent, SessionEvent } from "../../lib/types";

afterEach(cleanup);

const AT = "2026-06-02T12:00:00.000Z";
const AT2 = "2026-06-02T12:00:18.000Z";

function indexed(events: SessionEvent[]): IndexedEvent[] {
  return events.map((event, idx) => ({ idx, event }));
}

describe("SessionThread", () => {
  test("renders a completed run: user prompt + assistant reply reach the DOM", async () => {
    renderWithProviders(
      <SessionThread
        sessionId="s1"
        status="idle"
        events={indexed([
          {
            type: "agent_message",
            run_id: "",
            message_id: "u1",
            role: "user",
            text: "fix the flaky test",
            at: AT,
          },
          { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
          {
            type: "agent_message",
            run_id: "r1",
            message_id: "a1",
            role: "assistant",
            text: "on it",
            at: AT,
          },
          { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
        ])}
      />,
    );

    await waitFor(() => expect(screen.getByText("fix the flaky test")).toBeTruthy());
    expect(screen.getByText("on it")).toBeTruthy();
  });

  test("a shell exec renders as a $ command with its exit status", async () => {
    renderWithProviders(
      <SessionThread
        sessionId="s1"
        status="idle"
        events={indexed([
          { type: "exec_started", exec_id: "x1", command: ["cargo", "check"], at: AT },
          { type: "stdout", exec_id: "x1", chunk: "Compiling…\n" },
          {
            type: "exec_completed",
            exec_id: "x1",
            exit_status: 0,
            rusage: { duration_ms: 4200 },
            at: AT2,
          },
          { type: "harness_idle", at: AT2 },
        ])}
      />,
    );
    await waitFor(() => expect(screen.getByText("cargo check")).toBeTruthy());
    expect(screen.getByText("exit 0")).toBeTruthy();
  });

  test("an opened PR renders the harness-register card", async () => {
    renderWithProviders(
      <SessionThread
        sessionId="s1"
        status="idle"
        events={indexed([
          {
            type: "pull_request_opened",
            url: "https://gh/x/pull/7",
            repo: "x/y",
            title: "Fix the flaky test",
            number: 7,
            head_branch: "fix",
            base_branch: "main",
            at: AT,
          },
          { type: "harness_idle", at: AT2 },
        ])}
      />,
    );
    await waitFor(() => expect(screen.getByText("Fix the flaky test")).toBeTruthy());
    expect(screen.getByText("pull request")).toBeTruthy();
    expect(screen.getByText("x/y #7")).toBeTruthy();
  });

  test("a snapshot renders a durability marker", async () => {
    renderWithProviders(
      <SessionThread
        sessionId="s1"
        status="idle"
        events={indexed([
          { type: "snapshot_taken", snapshot_id: "s", size_bytes: 1_287_000_000, at: AT },
        ])}
      />,
    );
    await waitFor(() => expect(screen.getByText("snapshotted")).toBeTruthy());
  });

  test("a completed run renders its receipt footer with the tally", async () => {
    renderWithProviders(
      <SessionThread
        sessionId="s1"
        status="idle"
        events={indexed([
          { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
          {
            type: "tool_call_started",
            run_id: "r1",
            tool_call_id: "t1",
            tool_name: "Read",
            args_summary: '{"file_path":"a.rs"}',
            at: AT,
          },
          {
            type: "tool_call_completed",
            run_id: "r1",
            tool_call_id: "t1",
            tool_name: "Read",
            ok: true,
            duration_ms: 11,
            result_summary: "212 lines",
            at: AT,
          },
          { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
        ])}
      />,
    );
    await waitFor(() => expect(screen.getByText(/read 1/)).toBeTruthy());
  });

  // ADR 0054: the interactive AskUserQuestion card — the deferred question
  // renders a form; submitting POSTs answers keyed by question text (StringList
  // values) and flips to an optimistic receipt.
  test("a deferred question renders an interactive card; submitting POSTs the answer", async () => {
    let captured: {
      sessionId: string;
      toolCallId: string;
      answers: Record<string, string[]>;
    } | null = null;
    const transport = createRouterTransport((router) => {
      router.service(SessionService, {
        answerQuestion: (req) => {
          captured = {
            sessionId: req.sessionId,
            toolCallId: req.toolCallId,
            answers: Object.fromEntries(Object.entries(req.answers).map(([k, v]) => [k, v.values])),
          };
          return { sessionId: req.sessionId, note: "ok" };
        },
      });
    });

    const user = userEvent.setup();
    renderWithProviders(
      <SessionThread
        sessionId="s1"
        status="idle"
        events={indexed([
          { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
          {
            type: "tool_call_started",
            run_id: "r1",
            tool_call_id: "t1",
            tool_name: "AskUserQuestion",
            args_summary: null,
            at: AT,
          },
          {
            type: "user_question",
            run_id: "r1",
            tool_call_id: "t1",
            questions: [
              {
                question: "Which database?",
                header: "Database",
                multiSelect: false,
                options: [
                  { label: "Postgres", description: "Relational, default" },
                  { label: "MySQL", description: "Also relational" },
                ],
              },
            ],
            at: AT,
          },
          { type: "run_completed", run_id: "r1", ok: false, at: AT2 },
        ])}
      />,
      { transport },
    );

    await waitFor(() => expect(screen.getByText("Which database?")).toBeTruthy());
    // The generic AskUserQuestion tool part is suppressed — only the card shows.
    expect(screen.queryByText("AskUserQuestion")).toBeNull();

    // Submit is gated until a selection is made.
    const submit = screen.getByRole("button", { name: "Submit answer" });
    expect((submit as HTMLButtonElement).disabled).toBe(true);

    await user.click(screen.getByText("Postgres"));
    expect((submit as HTMLButtonElement).disabled).toBe(false);
    await user.click(submit);

    await waitFor(() => expect(captured).not.toBeNull());
    expect(captured!).toEqual({
      sessionId: "s1",
      toolCallId: "t1",
      answers: { "Which database?": ["Postgres"] },
    });
    // Optimistic receipt: the form is replaced while the answer round-trips.
    await waitFor(() => expect(screen.getByText("saving…")).toBeTruthy());
  });
});
