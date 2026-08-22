import { act, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { WebsocketProvider } from "y-websocket";
import * as Y from "yjs";

import { renderWithProviders } from "@/test-utils";
import { SpecShellPage } from "./SpecShellPage";

const readState = vi.hoisted(() => ({
  phase: "drafting" as "ideation" | "drafting" | "published",
}));

const providerState = vi.hoisted(() => {
  // Keyed by event name, like the real ObservableV2. A single undifferentiated
  // callback set would deliver `sync` payloads to the connection-failure
  // listeners, which is how a mock ends up dictating the production shape.
  const listeners = new Map<string, Set<(...args: never[]) => void>>();
  const listenersFor = (event: string) => {
    const existing = listeners.get(event);
    if (existing) return existing;
    const created = new Set<(...args: never[]) => void>();
    listeners.set(event, created);
    return created;
  };
  return {
    listeners,
    emit: (event: string, ...args: unknown[]) => {
      for (const callback of listenersFor(event)) {
        (callback as unknown as (...a: unknown[]) => void)(...args);
      }
    },
    provider: {
      synced: false,
      on: vi.fn((event: string, callback: (...args: never[]) => void) =>
        listenersFor(event).add(callback),
      ),
      off: vi.fn((event: string, callback: (...args: never[]) => void) =>
        listenersFor(event).delete(callback),
      ),
      connect: vi.fn(),
      disconnect: vi.fn(),
      destroy: vi.fn(),
    },
    create: vi.fn(),
    canvas: vi.fn(),
  };
});

vi.mock("@/components/spec/SpecConnection", () => ({
  createSpecConnection: (specId: string) => {
    const doc = new Y.Doc();
    providerState.create(specId, doc);
    return { doc, provider: providerState.provider as unknown as WebsocketProvider };
  },
}));

vi.mock("@/components/spec/LazySpecCanvas", () => ({
  LazySpecCanvas: (props: { doc: Y.Doc; provider: WebsocketProvider; specId: string }) => {
    providerState.canvas(props);
    return <div aria-label="Collaborative spec canvas">{props.specId}</div>;
  },
}));

vi.mock("@/components/spec-mode/SpecPublishedView", () => ({
  SpecPublishedView: (props: { title: string; openQuestions: unknown[] }) => (
    <main aria-label="Published spec view">
      {props.title} · {props.openQuestions.length} open
    </main>
  ),
}));

vi.mock("@/hooks/useSpecPublish", () => ({
  useSpecPublish: () => ({ data: { gate: { openQuestions: [] } } }),
}));
vi.mock("@/hooks/useSpecEvents", () => ({
  useSpecEvents: () => ({ events: [], streamingText: "", error: null, missed: 0 }),
}));

vi.mock("@/hooks/useSpecMessages", () => ({
  useSpecMessages: () => ({
    data: { messages: [], byPromptId: new Map(), nextAfter: null },
    error: null,
  }),
  useSendSpecMessage: () => ({
    mutateAsync: vi.fn(async () => ({ promptId: "prompt-1" })),
  }),
}));

vi.mock("@/hooks/useSpecRead", () => ({
  useSpecRead: () => ({
    data: {
      spec: {
        id: "spec-1",
        title: "Quota design",
        phase: readState.phase,
        sessionId: null,
        viewerIsOwner: false,
        publishedCheckpointId: readState.phase === "published" ? "published-1" : null,
        publishedAt: readState.phase === "published" ? "2026-08-13T18:00:00.000Z" : null,
        revision: "17",
        template: { id: "template-1", name: "Engineering spec" },
      },
      checkpoints:
        readState.phase === "published"
          ? [
              {
                id: "published-1",
                label: "Published",
                author: { id: "owner", name: "Nikhil" },
                reason: "publish",
                docSeq: "17",
                createdAt: "2026-08-13T18:00:00.000Z",
              },
            ]
          : [],
      publishedCheckpoint:
        readState.phase === "published"
          ? {
              id: "published-1",
              label: "Published",
              authorUserId: "owner",
              reason: "publish",
              docSeq: "17",
              createdAt: "2026-08-13T18:00:00.000Z",
              markdown: "## Problem\n\nPublished.\n",
              sections: [{ id: "problem", title: "Problem" }],
            }
          : null,
    },
    isPending: false,
    error: null,
  }),
  useStartSpecDrafting: () => ({
    isPending: false,
    error: null,
    mutate: vi.fn(),
  }),
  useSpecRail: () => ({
    data: {
      sections: [
        {
          id: "problem",
          templateKey: "problem",
          title: "Problem",
          state: "open",
          naReason: null,
          allowNa: true,
          openQuestionCount: 0,
          settledBy: null,
          stateChangedAt: null,
        },
      ],
      completeness: { complete: 0, total: 1 },
    },
  }),
  useSpecCheckpoint: () => ({ data: undefined, isPending: false }),
}));

describe("SpecShellPage", () => {
  beforeEach(() => {
    readState.phase = "drafting";
  });

  // Several cases below spy on the GLOBAL fetch. An inline `mockRestore()` at
  // the end of each is not enough: a failing assertion throws before it, and
  // the spy then leaks into whatever runs next. That showed up as two
  // unrelated suites failing in one full run and passing in isolation.
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("owns one Yjs connection and disposes it after the canvas unmounts", async () => {
    providerState.listeners.clear();
    providerState.provider.on.mockClear();
    providerState.provider.off.mockClear();
    providerState.provider.destroy.mockClear();
    providerState.create.mockClear();
    providerState.canvas.mockClear();

    const view = renderWithProviders(<SpecShellPage specId="spec-1" />);
    await waitFor(() => expect(providerState.create).toHaveBeenCalledTimes(1));
    expect(await screen.findByLabelText("Loading collaborative spec")).toBeTruthy();

    act(() => {
      providerState.emit("sync", true);
    });

    expect(await screen.findByLabelText("Collaborative spec canvas")).toBeTruthy();
    const createdDoc = providerState.create.mock.calls[0]?.[1];
    expect(providerState.canvas).toHaveBeenCalledWith(
      expect.objectContaining({
        doc: createdDoc,
        provider: providerState.provider,
        specId: "spec-1",
      }),
    );

    view.unmount();
    // One `off` per listener the hook registered: sync, connection-close, closed.
    expect(providerState.provider.off).toHaveBeenCalledTimes(3);
    expect(providerState.provider.destroy).toHaveBeenCalledTimes(1);
    expect((createdDoc as Y.Doc).isDestroyed).toBe(true);
  });

  // y-websocket fires BOTH onerror and onclose for one failed handshake, so a
  // real attempt is modelled as both here: the pane must count it once.
  const failedAttempt = () => {
    providerState.emit("connection-error", new Event("error"));
    providerState.emit("connection-close", new CloseEvent("close", { code: 1006 }));
  };

  // A dead document socket used to render as loading skeletons forever, so a
  // reader could not tell "slow" from "will never connect". Prod ran that way
  // for every spec.
  it("says the document is unreachable after repeated handshake failures", async () => {
    providerState.listeners.clear();
    providerState.provider.disconnect.mockClear();
    providerState.provider.connect.mockClear();

    renderWithProviders(<SpecShellPage specId="spec-1" />);
    await waitFor(() => expect(providerState.listeners.has("connection-close")).toBe(true));

    // Below the threshold the pane stays quiet — one blip is not a failure.
    act(() => {
      failedAttempt();
      failedAttempt();
    });
    expect(screen.getByLabelText("Loading collaborative spec")).toBeTruthy();

    act(() => {
      failedAttempt();
    });
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(screen.getByText("Cannot reach the document")).toBeTruthy();

    // Keeps trying for a while, then stops instead of spinning forever.
    act(() => {
      for (let attempt = 0; attempt < 7; attempt += 1) failedAttempt();
    });
    expect(screen.getByText(/stopped trying/)).toBeTruthy();
    // Deferred to a microtask to break the re-entrancy, so await it.
    await waitFor(() => expect(providerState.provider.disconnect).toHaveBeenCalledTimes(1));

    screen.getByRole("button", { name: "Try again" }).click();
    await waitFor(() => expect(providerState.provider.connect).toHaveBeenCalledTimes(1));
  });

  // `disconnect()` re-enters y-websocket's closeWebsocketConnection, which
  // re-emits `connection-close` BEFORE clearing `provider.ws`. Calling it from
  // inside that handler recursed until the stack blew — seen in prod as
  // `RangeError: Maximum call stack size exceeded`.
  it("does not recurse when giving up triggers a disconnect", async () => {
    providerState.listeners.clear();
    providerState.provider.disconnect.mockReset();
    providerState.provider.disconnect.mockImplementation(() => {
      providerState.emit("connection-close", new CloseEvent("close", { code: 1006 }));
    });

    renderWithProviders(<SpecShellPage specId="spec-1" />);
    await waitFor(() => expect(providerState.listeners.has("connection-close")).toBe(true));

    act(() => {
      for (let attempt = 0; attempt < 12; attempt += 1) failedAttempt();
    });

    expect(screen.getByText(/stopped trying/)).toBeTruthy();
    await waitFor(() => expect(providerState.provider.disconnect).toHaveBeenCalledTimes(1));
    providerState.provider.disconnect.mockReset();
  });

  // The error and close pair used to increment twice, so the pane gave up
  // after five attempts while claiming ten.
  it("counts one failure per attempt, not one per event", async () => {
    providerState.listeners.clear();
    providerState.provider.disconnect.mockClear();

    renderWithProviders(<SpecShellPage specId="spec-1" />);
    await waitFor(() => expect(providerState.listeners.has("connection-close")).toBe(true));

    // Nine attempts: visible, but not yet given up.
    act(() => {
      for (let attempt = 0; attempt < 9; attempt += 1) failedAttempt();
    });
    expect(screen.getByText("Cannot reach the document")).toBeTruthy();
    expect(screen.queryByText(/stopped trying/)).toBeNull();
    expect(providerState.provider.disconnect).not.toHaveBeenCalled();

    act(() => {
      failedAttempt();
    });
    expect(screen.getByText(/stopped trying/)).toBeTruthy();
    await waitFor(() => expect(providerState.provider.disconnect).toHaveBeenCalledTimes(1));
  });

  // The load balancer usually eats the refusal close code (measured against
  // prod: delivered once in eight attempts, and a pre-I/O refusal loses even
  // the 101). So the reason has to come from the HTTP API, which crosses the
  // same proxy without any of this.
  it("asks the API why, when the close code never arrives", async () => {
    providerState.listeners.clear();
    const fetchMock = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValue(new Response(null, { status: 401 }));

    renderWithProviders(<SpecShellPage specId="spec-1" />);
    await waitFor(() => expect(providerState.listeners.has("connection-close")).toBe(true));

    // Three transport failures, no close code — exactly what prod produces.
    act(() => {
      failedAttempt();
      failedAttempt();
      failedAttempt();
    });

    expect(await screen.findByText("Your session expired")).toBeTruthy();
    expect(fetchMock).toHaveBeenCalledWith(
      expect.stringContaining("/specs/spec-1"),
      expect.objectContaining({ credentials: "include" }),
    );
    fetchMock.mockRestore();
  });

  // A mount-lifetime latch would spend its one classification on the first
  // blip. The reason can change while the tab stays open — a cookie expiring
  // mid-session is the ordinary case — so every failure episode must be able
  // to ask again.
  it("classifies a later failure episode after the socket recovered", async () => {
    providerState.listeners.clear();
    // Episode 1: the spec is fine, so the API declines to override the
    // transport verdict.
    const fetchMock = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValue(
        new Response(JSON.stringify({ spec: { phase: "drafting" } }), { status: 200 }),
      );

    renderWithProviders(<SpecShellPage specId="spec-1" />);
    await waitFor(() => expect(providerState.listeners.has("connection-close")).toBe(true));
    act(() => {
      failedAttempt();
      failedAttempt();
      failedAttempt();
    });
    expect(await screen.findByText("Cannot reach the document")).toBeTruthy();
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));

    // The socket recovers.
    act(() => {
      providerState.emit("sync", true);
    });
    expect(await screen.findByLabelText("Collaborative spec canvas")).toBeTruthy();

    // Episode 2: now the session has expired.
    providerState.provider.synced = false;
    fetchMock.mockResolvedValue(new Response(null, { status: 401 }));
    act(() => {
      providerState.emit("sync", false);
      failedAttempt();
      failedAttempt();
      failedAttempt();
    });

    expect(await screen.findByText("Your session expired")).toBeTruthy();
    providerState.provider.synced = false;
    fetchMock.mockRestore();
  });

  // Caught driving prod, not by these tests: the classifier's answer lands
  // asynchronously while the socket is STILL failing, so the next
  // connection-close re-set "unreachable" and wiped the reason a moment after
  // it appeared. The other cases emit exactly the threshold count and stop, so
  // they could never see it.
  it("does not let further failures clobber a classified refusal", async () => {
    providerState.listeners.clear();
    const fetchMock = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValue(new Response(null, { status: 401 }));

    renderWithProviders(<SpecShellPage specId="spec-1" />);
    await waitFor(() => expect(providerState.listeners.has("connection-close")).toBe(true));
    act(() => {
      failedAttempt();
      failedAttempt();
      failedAttempt();
    });
    expect(await screen.findByText("Your session expired")).toBeTruthy();

    // The socket keeps failing after the reason is known.
    act(() => {
      failedAttempt();
      failedAttempt();
      failedAttempt();
    });
    expect(screen.getByText("Your session expired")).toBeTruthy();
    expect(screen.queryByText("Cannot reach the document")).toBeNull();
    fetchMock.mockRestore();
  });

  it("keeps the transport verdict when the API is unreachable too", async () => {
    providerState.listeners.clear();
    const fetchMock = vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("offline"));

    renderWithProviders(<SpecShellPage specId="spec-1" />);
    await waitFor(() => expect(providerState.listeners.has("connection-close")).toBe(true));
    act(() => {
      failedAttempt();
      failedAttempt();
      failedAttempt();
    });

    expect(await screen.findByText("Cannot reach the document")).toBeTruthy();
    fetchMock.mockRestore();
  });

  it("names an application close code instead of retrying a refusal", async () => {
    providerState.listeners.clear();
    providerState.provider.disconnect.mockClear();

    renderWithProviders(<SpecShellPage specId="spec-1" />);
    await waitFor(() => expect(providerState.listeners.has("closed")).toBe(true));

    // y-websocket emits `connection-close` before `closed`; the refusal must
    // win over the transient state that sets.
    act(() => {
      providerState.emit("connection-close", new CloseEvent("close", { code: 4401 }));
      providerState.emit("closed", { code: 4401, reason: "spec sync 401" });
    });

    expect(await screen.findByText("Your session expired")).toBeTruthy();
    // A refusal is a decision, so it must not offer a pointless retry.
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
    expect(providerState.provider.disconnect).toHaveBeenCalledTimes(1);
  });

  it("routes an ideation server phase to the centered thread instead of the shell", async () => {
    readState.phase = "ideation";

    renderWithProviders(<SpecShellPage specId="spec-1" />);

    expect(await screen.findByRole("main", { name: "Spec ideation" })).toBeTruthy();
    expect(screen.getByText("Thinking it through")).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Spec document" })).toBeNull();
    expect(screen.queryByRole("complementary", { name: "Spec sections" })).toBeNull();
  });

  it("routes a published phase to the artifact read view", async () => {
    readState.phase = "published";

    renderWithProviders(<SpecShellPage specId="spec-1" />);

    expect(await screen.findByRole("main", { name: "Published spec view" })).toBeTruthy();
    expect(screen.getByText("Quota design · 0 open")).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Spec document" })).toBeNull();
  });
});
