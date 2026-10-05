// The transcript's fenced code blocks, exercised through the real path: an SSE
// `agent_message` -> SessionThread -> the assistant-ui runtime -> MarkdownText.
//
// This is the test that was missing. CodeBlock has its own unit tests, and they
// passed while the transcript stayed plain, because the bug was not in the
// highlighter at all: `memoizeMarkdownComponents` wraps the `pre` and `code`
// components in `React.memo` with a comparator that reads ONLY the markdown
// node, which never changes after a fence is parsed. Rendering tokens through
// them meant rendering into a subtree React had stopped updating. Nothing below
// the component boundary can catch that — only mounting the real transcript can.

import { afterEach, describe, expect, test } from "vitest";
import { cleanup, waitFor } from "@testing-library/react";

import { SessionThread } from "@/components/session-thread/SessionThread";
import { renderWithProviders } from "@/test-utils";
import type { IndexedEvent, SessionEvent } from "@/lib/types";

afterEach(cleanup);

const AT = "2026-06-02T12:00:00.000Z";
const AT2 = "2026-06-02T12:00:18.000Z";

const indexed = (events: SessionEvent[]): IndexedEvent[] =>
  events.map((event, idx) => ({ idx, event }));

function transcript(text: string) {
  return renderWithProviders(
    <SessionThread
      sessionId="s1"
      status="idle"
      events={indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text,
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ])}
    />,
  );
}

const fence = (lang: string, body: string) => ["```" + lang, body, "```"].join("\n");

describe("transcript code fences", () => {
  test("a fence with a language highlights", async () => {
    const { container } = transcript(
      fence("kotlin", "data class GetCatalogsResponse(val catalogs: List<CatalogPageResponse>)"),
    );

    const code = () => container.querySelector("pre code") as HTMLElement | null;
    await waitFor(() => expect(code()).toBeTruthy());
    expect(code()!.dataset["language"]).toBe("kotlin");

    // The tokens arrive asynchronously — after the markdown node is final, which
    // is exactly the window the memoized components used to swallow.
    await waitFor(
      () => {
        expect(code()!.dataset["highlighted"]).toBe("true");
      },
      { timeout: 5000 },
    );
    expect(code()!.querySelectorAll("span[style*='--syntax-light']").length).toBeGreaterThan(1);
    expect(code()!.textContent).toContain("GetCatalogsResponse");
  });

  test("a fence with no language stays plain", async () => {
    const { container } = transcript(fence("", "poetry run cortex catalogs list"));

    const code = () => container.querySelector("pre code") as HTMLElement | null;
    await waitFor(() => expect(code()).toBeTruthy());
    await new Promise((resolve) => setTimeout(resolve, 300));
    expect(code()!.dataset["highlighted"]).toBe("false");
    expect(code()!.querySelector("span[style*='--syntax-light']")).toBeNull();
    expect(code()!.textContent).toContain("poetry run cortex");
  });
});
