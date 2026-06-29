// Render + lazy-import smoke for the BROWSER tab viewer (ADR 0064).
//
// The real `@novnc/novnc` RFB client touches WebSocket + canvas + WASM-ish
// rendering paths that jsdom can't run, so we mock the module with a stub RFB
// class. The point is to pin: (1) the initial loading state renders before the
// dynamic import resolves, (2) the lazy import resolves and constructs an RFB
// against the `/sessions/:id/vnc` WS URL with the driving viewer config, and
// (3) unmount disconnects it.

import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { act, cleanup, render, screen } from "@testing-library/react";
import { BrowserPane } from "./BrowserPane";

// vi.hoisted runs before the vi.mock factory so the class exists when the mock
// module is constructed AND is reachable from the test bodies.
const { MockRFB } = vi.hoisted(() => {
  class MockRFB {
    static instances: MockRFB[] = [];
    viewOnly = true;
    scaleViewport = false;
    resizeSession = false;
    addEventListener = vi.fn();
    removeEventListener = vi.fn();
    disconnect = vi.fn();
    constructor(
      public target: HTMLElement,
      public url: string,
      public options?: unknown,
    ) {
      MockRFB.instances.push(this);
    }
  }
  return { MockRFB };
});

vi.mock("@novnc/novnc", () => ({ default: MockRFB }));

beforeEach(() => {
  MockRFB.instances = [];
});

afterEach(() => {
  cleanup();
});

async function flush() {
  // The mount effect awaits loadRfb() (a microtask chain). A couple of act()
  // turns drain the resolved import promise and the body that runs after it.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

describe("BrowserPane", () => {
  test("renders the loading state on first render", () => {
    render(<BrowserPane sessionId="s1" />);
    expect(screen.getByText(/loading browser viewer/i)).toBeTruthy();
  });

  test("lazy-imports noVNC and constructs an RFB against /sessions/:id/vnc as a driving viewer", async () => {
    render(<BrowserPane sessionId="abc-123" />);
    await flush();

    expect(MockRFB.instances).toHaveLength(1);
    const rfb = MockRFB.instances[0]!;
    expect(rfb.url).toContain("/sessions/abc-123/vnc");
    expect(rfb.viewOnly).toBe(false);
    expect(rfb.scaleViewport).toBe(true);
    expect(rfb.resizeSession).toBe(true);
  });

  test("disconnects the RFB session on unmount", async () => {
    const { unmount } = render(<BrowserPane sessionId="s1" />);
    await flush();
    const rfb = MockRFB.instances.at(-1)!;
    unmount();
    expect(rfb.disconnect).toHaveBeenCalledTimes(1);
  });
});
