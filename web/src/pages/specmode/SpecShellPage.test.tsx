import { act, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { WebsocketProvider } from "y-websocket";
import * as Y from "yjs";

import { renderWithProviders } from "@/test-utils";
import { SpecShellPage } from "./SpecShellPage";

const providerState = vi.hoisted(() => {
  const callbacks = new Set<(synced: boolean) => void>();
  return {
    callbacks,
    provider: {
      synced: false,
      on: vi.fn((_event: string, callback: (synced: boolean) => void) => callbacks.add(callback)),
      off: vi.fn((_event: string, callback: (synced: boolean) => void) =>
        callbacks.delete(callback),
      ),
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

vi.mock("@/hooks/useSpecRead", () => ({
  useSpecRead: () => ({
    data: {
      spec: {
        id: "spec-1",
        title: "Quota design",
        lifecycle: "draft",
        sessionId: null,
        viewerIsOwner: false,
        publishedCheckpointId: null,
        publishedAt: null,
        revision: "17",
        template: { id: "template-1", name: "Engineering spec" },
      },
      checkpoints: [],
      publishedCheckpoint: null,
    },
    isPending: false,
    error: null,
  }),
  useSpecCheckpoint: () => ({ data: undefined, isPending: false }),
}));

describe("SpecShellPage", () => {
  it("owns one Yjs connection and disposes it after the canvas unmounts", async () => {
    providerState.callbacks.clear();
    providerState.provider.on.mockClear();
    providerState.provider.off.mockClear();
    providerState.provider.destroy.mockClear();
    providerState.create.mockClear();
    providerState.canvas.mockClear();

    const view = renderWithProviders(<SpecShellPage specId="spec-1" />);
    await waitFor(() => expect(providerState.create).toHaveBeenCalledTimes(1));
    expect(await screen.findByLabelText("Loading collaborative spec")).toBeTruthy();

    act(() => {
      for (const callback of providerState.callbacks) callback(true);
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
    expect(providerState.provider.off).toHaveBeenCalledTimes(1);
    expect(providerState.provider.destroy).toHaveBeenCalledTimes(1);
    expect((createdDoc as Y.Doc).isDestroyed).toBe(true);
  });
});
