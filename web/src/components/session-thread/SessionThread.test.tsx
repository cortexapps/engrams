// Smoke test: mount SessionThread through the external-store runtime and
// confirm the SSE-derived messages actually reach the DOM. We feed a
// completed run (non-busy) so no loop animation / Web-Animations path mounts
// under jsdom.

import { useState } from "react";
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

// Regression harness for the cross-session optimistic-bubble bleed: the sessions
// rail navigates by path param only, so `SessionThread` is NOT remounted on a
// session switch — its optimistic `pending` array is shared across every session
// visited in one page load. This flips `sessionId` on a SINGLE instance (no
// `key` → no remount), exactly mirroring that navigation; each pending entry must
// be scoped to the session it was sent to.
function NavHarness() {
  const [sid, setSid] = useState("session-A");
  return (
    <>
      <button onClick={() => setSid("session-A")}>nav-A</button>
      <button onClick={() => setSid("session-B")}>nav-B</button>
      <SessionThread
        sessionId={sid}
        status="idle"
        events={
          sid === "session-A"
            ? []
            : indexed([
                { type: "run_started", run_id: "rb", prompt_summary: null, at: AT },
                {
                  type: "agent_message",
                  run_id: "rb",
                  message_id: "ab",
                  role: "assistant",
                  text: "B's own reply",
                  at: AT,
                },
                { type: "run_completed", run_id: "rb", ok: true, at: AT2 },
              ])
        }
      />
    </>
  );
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
            type: "integration_asset",
            provider: "forge",
            asset_kind: "pull_request",
            surface: "asset",
            data: {
              repo: "x/y",
              title: "Fix the flaky test",
              number: 7,
              head_branch: "fix",
              base_branch: "main",
            },
            fetchable: { kind: "external", url: "https://gh/x/pull/7" },
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

  test("an unanswered legacy question is read-only after the protocol upgrade", async () => {
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
    );

    await waitFor(() => expect(screen.getByText("Which database?")).toBeTruthy());
    // The generic AskUserQuestion tool part is suppressed — only the card shows.
    expect(screen.queryByText("AskUserQuestion")).toBeNull();

    const submit = screen.getByRole("button", { name: "Submit answer" });
    expect((submit as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("radio", { name: /Postgres/ }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect(screen.getByText(/predates an upgrade/i)).toBeTruthy();
  });

  test("a generic question submits canonical answers through CompleteToolCall only", async () => {
    let completed: { sessionId: string; toolCallId: string; resultJson: string } | null = null;
    const transport = createRouterTransport((router) => {
      router.service(SessionService, {
        completeToolCall: (req) => {
          completed = {
            sessionId: req.sessionId,
            toolCallId: req.toolCallId,
            resultJson: req.resultJson,
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
          {
            type: "tool_call_requested",
            run_id: "r1",
            tool_call_id: "t-generic",
            name: "ask_user_question",
            args_json: JSON.stringify({
              questions: [
                {
                  question: "Which database?",
                  header: "Database",
                  multiSelect: false,
                  options: [{ label: "Postgres", description: "Relational, default" }],
                },
              ],
            }),
            at: AT,
          },
        ])}
      />,
      { transport },
    );

    await user.click(await screen.findByText("Postgres"));
    await user.click(screen.getByRole("button", { name: "Submit answer" }));

    await waitFor(() => expect(completed).not.toBeNull());
    expect(completed).toEqual({
      sessionId: "s1",
      toolCallId: "t-generic",
      resultJson: JSON.stringify({ "Which database?": ["Postgres"] }),
    });
    expect(await screen.findByText("saving…")).toBeTruthy();
  });

  test("an unanswered non-question generic call shows a waiting tool affordance", async () => {
    renderWithProviders(
      <SessionThread
        sessionId="s1"
        status="idle"
        events={indexed([
          {
            type: "tool_call_requested",
            run_id: "r1",
            tool_call_id: "approval-1",
            name: "approve_deploy",
            args_json: JSON.stringify({ environment: "production" }),
            at: AT,
          },
        ])}
      />,
    );

    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Waiting for tool: approve_deploy" })).toBeTruthy(),
    );
  });

  // Regression: an idle send to session A must not bleed its optimistic bubble
  // into session B when the same (un-remounted) SessionThread navigates there,
  // and the bubble must reappear on returning to A (it survives navigation; it's
  // reset only on a full page load).
  test("an optimistic prompt is scoped to its session: no cross-session bleed, survives nav-back", async () => {
    const user = userEvent.setup();
    renderWithProviders(<NavHarness />);

    // Send an idle prompt while viewing session A (⌘/Ctrl+↵ submits).
    const input = await screen.findByLabelText("Message input");
    await user.click(input);
    await user.type(input, "bleed-probe-A");
    await user.keyboard("{Control>}{Enter}{/Control}");
    await waitFor(() => expect(screen.getByText("bleed-probe-A")).toBeTruthy());

    // Navigate to session B (same instance, no remount): the bubble must NOT bleed.
    await user.click(screen.getByText("nav-B"));
    await waitFor(() => expect(screen.getByText("B's own reply")).toBeTruthy());
    expect(screen.queryByText("bleed-probe-A")).toBeNull();

    // Back to A: the optimistic bubble is still there.
    await user.click(screen.getByText("nav-A"));
    await waitFor(() => expect(screen.getByText("bleed-probe-A")).toBeTruthy());
  });
});
