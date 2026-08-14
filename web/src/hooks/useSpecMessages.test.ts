import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { afterEach, describe, expect, test, vi } from "vitest";

import {
  getSpecMessages,
  mergeSpecMessagePages,
  postSpecMessage,
  type SpecMessage,
  useSendSpecMessage,
} from "./useSpecMessages";

afterEach(() => vi.unstubAllGlobals());

describe("spec message clients", () => {
  test("reads a deleted-author snapshot and echoes opaque server cursors unchanged", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        jsonResponse({
          messages: [
            {
              prompt_id: "prompt-1",
              author: { id: null, name: "Priya" },
              text: "Keep one parent bucket.",
              created_at: "2026-08-13T10:00:00.000Z",
            },
          ],
          next_after: "opaque:next/with spaces?and=punctuation",
        }),
      )
      .mockResolvedValueOnce(
        jsonResponse({ messages: [], next_after: "opaque:next/with spaces?and=punctuation" }),
      );
    vi.stubGlobal("fetch", fetchMock);

    const result = await getSpecMessages("spec/1");

    expect(fetchMock).toHaveBeenNthCalledWith(
      1,
      "/api/v1/specs/spec%2F1/messages?after=",
      expect.objectContaining({ credentials: "include" }),
    );
    expect(result.messages[0]).toEqual({
      promptId: "prompt-1",
      author: { id: null, name: "Priya" },
      text: "Keep one parent bucket.",
      createdAt: "2026-08-13T10:00:00.000Z",
    });
    expect(result.byPromptId.get("prompt-1")).toBe(result.messages[0]);
    expect(result.nextAfter).toBe("opaque:next/with spaces?and=punctuation");

    await getSpecMessages("spec/1", result.nextAfter);
    expect(fetchMock).toHaveBeenNthCalledWith(
      2,
      "/api/v1/specs/spec%2F1/messages?after=opaque%3Anext%2Fwith%20spaces%3Fand%3Dpunctuation",
      expect.objectContaining({ credentials: "include" }),
    );
  });

  test("merges pages by prompt id and keeps the server's last opaque cursor", () => {
    const first = message("prompt-1", "First");
    const boundary = message("prompt-2", "Boundary");
    const repeatedBoundary = message("prompt-2", "Boundary");
    const last = message("prompt-3", "Last");

    const result = mergeSpecMessagePages([
      { messages: [first, boundary], nextAfter: "opaque:first" },
      { messages: [repeatedBoundary, last], nextAfter: "opaque:last" },
    ]);

    expect(result.messages.map(({ promptId }) => promptId)).toEqual([
      "prompt-1",
      "prompt-2",
      "prompt-3",
    ]);
    expect(result.byPromptId.get("prompt-2")).toBe(repeatedBoundary);
    expect(result.nextAfter).toBe("opaque:last");
  });

  test("posts only clean message text and returns the prompt id", async () => {
    const fetchMock = vi.fn(() => Promise.resolve(jsonResponse({ prompt_id: "prompt-2" }, 202)));
    vi.stubGlobal("fetch", fetchMock);

    await expect(postSpecMessage("spec-1", "One shared limit.")).resolves.toEqual({
      promptId: "prompt-2",
    });
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/specs/spec-1/messages",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ message: "One shared limit." }),
      }),
    );
  });

  test("invalidates every message cursor after a successful send", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(jsonResponse({ prompt_id: "prompt-3" }, 202))),
    );
    const client = new QueryClient({ defaultOptions: { mutations: { retry: false } } });
    const invalidate = vi.spyOn(client, "invalidateQueries");
    const wrapper = ({ children }: { children: ReactNode }) =>
      createElement(QueryClientProvider, { client }, children);
    const { result } = renderHook(() => useSendSpecMessage("spec-1"), { wrapper });

    await result.current.mutateAsync("Write this down.");
    await waitFor(() =>
      expect(invalidate).toHaveBeenCalledWith({ queryKey: ["spec", "spec-1", "messages"] }),
    );
    client.clear();
  });
});

function jsonResponse(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function message(promptId: string, text: string): SpecMessage {
  return {
    promptId,
    author: { id: "member-1", name: "Ada" },
    text,
    createdAt: "2026-08-13T10:00:00.000Z",
  };
}
