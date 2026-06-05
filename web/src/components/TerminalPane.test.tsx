// Regression tests for the SHELL-tab "hangs on remount" bug.
//
// Symptom: open SHELL on an active session, navigate away via SPA,
// navigate back, click SHELL, type a command — the keystrokes flow
// over the websocket but no output renders. The underlying error is
// `RuntimeError: memory access out of bounds` thrown from inside
// term.write() the first time the new mount tries to render a chunk
// of any real size (e.g. the colorized output of `ls /`).
//
// Cause: ghostty-web 0.4 Terminal.dispose() already frees the WASM
// terminal via cleanupComponents(). An earlier version of this
// component called wasmTerm.free() ourselves first, which made
// dispose()'s subsequent free a *double-free* — that corrupted the
// allocator's free list and the next Terminal's grid landed on top
// of poisoned memory. Small writes survived; a multi-line write hit
// the corrupted region and threw.
//
// The mount-side workaround (clear-after-fit) addresses a separate,
// older "stale glyphs on remount" bug and is preserved.
//
// Both bugs have come back from "fixed" before because nothing pinned
// the cleanup contract. These tests assert it via mock invocation
// counters so a future refactor fails loudly.

import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import { TerminalPane } from "./TerminalPane";

// vi.hoisted runs before the vi.mock factory, so the classes are
// defined when the mock module is constructed AND accessible inside
// the test bodies via the returned references.
const { MockTerminal, MockFitAddon } = vi.hoisted(() => {
  class MockTerminal {
    static instances: MockTerminal[] = [];
    cols = 80;
    rows = 24;
    open = vi.fn();
    write = vi.fn();
    loadAddon = vi.fn();
    dispose = vi.fn();
    onData = vi.fn();
    onResize = vi.fn();
    wasmTerm = { free: vi.fn() };
    constructor() {
      MockTerminal.instances.push(this);
    }
  }
  class MockFitAddon {
    static instances: MockFitAddon[] = [];
    fit = vi.fn();
    observeResize = vi.fn();
    dispose = vi.fn();
    constructor() {
      MockFitAddon.instances.push(this);
    }
  }
  return { MockTerminal, MockFitAddon };
});

vi.mock("ghostty-web", () => ({
  init: vi.fn(async () => {}),
  Terminal: MockTerminal,
  FitAddon: MockFitAddon,
}));

class MockWebSocket {
  static instances: MockWebSocket[] = [];
  readyState = 0;
  onopen: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  binaryType = "blob";
  send = vi.fn();
  close = vi.fn();
  constructor(
    public url: string,
    public protocols?: string | string[],
  ) {
    MockWebSocket.instances.push(this);
  }
}

beforeEach(() => {
  MockTerminal.instances = [];
  MockFitAddon.instances = [];
  MockWebSocket.instances = [];
  vi.stubGlobal("WebSocket", MockWebSocket as unknown as typeof WebSocket);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function flush() {
  // The mount effect awaits loadGhostty() (a microtask chain). One
  // act() turn drains the resolved promises and the synchronous body
  // that runs after them.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

describe("TerminalPane mount sequence", () => {
  test("clear escape runs AFTER fitAddon.fit() so post-resize cells are zeroed", async () => {
    render(<TerminalPane sessionId="s1" />);
    await flush();

    const term = MockTerminal.instances.at(-1)!;
    const addon = MockFitAddon.instances.at(-1)!;

    expect(term.open).toHaveBeenCalledTimes(1);
    expect(addon.fit).toHaveBeenCalledTimes(1);
    expect(term.write).toHaveBeenCalledWith("\x1b[2J\x1b[3J\x1b[H");

    // Order: open → fit → write(clear). If a future change moves the
    // clear above fit, fit() would resize the grid afterward and the
    // cells it added would come back stale.
    const openOrder = term.open.mock.invocationCallOrder[0];
    const fitOrder = addon.fit.mock.invocationCallOrder[0];
    const clearWriteCall = term.write.mock.calls.findIndex(
      ([arg]) => arg === "\x1b[2J\x1b[3J\x1b[H",
    );
    const clearOrder = term.write.mock.invocationCallOrder[clearWriteCall];

    expect(openOrder).toBeLessThan(fitOrder);
    expect(fitOrder).toBeLessThan(clearOrder);
  });

  test("opens a WebSocket to /sessions/:id/shell with the tty subprotocol", async () => {
    render(<TerminalPane sessionId="abc-123" />);
    await flush();

    expect(MockWebSocket.instances).toHaveLength(1);
    const ws = MockWebSocket.instances[0]!;
    expect(ws.url).toContain("/sessions/abc-123/shell");
    expect(ws.protocols).toBe("tty");
  });
});

describe("TerminalPane unmount sequence", () => {
  test("disposes the Terminal but does NOT call wasmTerm.free() — that path double-frees", async () => {
    const { unmount } = render(<TerminalPane sessionId="s1" />);
    await flush();

    const term = MockTerminal.instances.at(-1)!;
    const addon = MockFitAddon.instances.at(-1)!;
    const ws = MockWebSocket.instances.at(-1)!;

    unmount();

    expect(ws.close).toHaveBeenCalledTimes(1);
    expect(addon.dispose).toHaveBeenCalledTimes(1);
    expect(term.dispose).toHaveBeenCalledTimes(1);

    // Terminal.dispose() runs cleanupComponents() which frees the
    // wasmTerm itself. Calling it from here too is the double-free
    // that lands the next mount on a corrupted heap and crashes
    // term.write() partway through `ls /` output.
    expect(term.wasmTerm.free).not.toHaveBeenCalled();
  });
});

describe("TerminalPane remount cycle", () => {
  test("a fresh mount allocates a new Terminal and re-runs the fit-then-clear sequence", async () => {
    const { unmount } = render(<TerminalPane sessionId="s1" />);
    await flush();

    const firstTerm = MockTerminal.instances.at(-1)!;
    expect(firstTerm.write).toHaveBeenCalledWith("\x1b[2J\x1b[3J\x1b[H");

    unmount();

    // Second mount — same sessionId, same as a SPA route nav out and
    // back. Must produce a fresh Terminal instance, not reuse the old.
    render(<TerminalPane sessionId="s1" />);
    await flush();

    const secondTerm = MockTerminal.instances.at(-1)!;
    const secondAddon = MockFitAddon.instances.at(-1)!;
    expect(secondTerm).not.toBe(firstTerm);
    expect(secondTerm.open).toHaveBeenCalledTimes(1);
    expect(secondAddon.fit).toHaveBeenCalledTimes(1);
    expect(secondTerm.write).toHaveBeenCalledWith("\x1b[2J\x1b[3J\x1b[H");

    // And the second mount's clear is still post-fit.
    const fitOrder = secondAddon.fit.mock.invocationCallOrder[0];
    const clearWriteCall = secondTerm.write.mock.calls.findIndex(
      ([arg]) => arg === "\x1b[2J\x1b[3J\x1b[H",
    );
    const clearOrder = secondTerm.write.mock.invocationCallOrder[clearWriteCall];
    expect(fitOrder).toBeLessThan(clearOrder);
  });

  test("regression: navigate-away-and-back never explicitly frees wasmTerm (would corrupt the next mount)", async () => {
    // Mirrors the user repro: open SHELL, navigate to overview via
    // SPA (unmount), navigate back (remount), click SHELL again, type
    // `ls /`. Pre-fix, the explicit wasmTerm.free() in the unmount
    // path double-freed against Terminal.dispose()'s own free, and
    // the second mount's first multi-line write OOB'd inside the
    // ghostty-web WASM. Asserting the cleanup never touches
    // wasmTerm.free() pins the contract that prevents it.
    const { unmount: unmount1 } = render(<TerminalPane sessionId="s1" />);
    await flush();
    const firstTerm = MockTerminal.instances.at(-1)!;
    unmount1();
    expect(firstTerm.dispose).toHaveBeenCalledTimes(1);
    expect(firstTerm.wasmTerm.free).not.toHaveBeenCalled();

    const { unmount: unmount2 } = render(<TerminalPane sessionId="s1" />);
    await flush();
    const secondTerm = MockTerminal.instances.at(-1)!;
    unmount2();
    expect(secondTerm.dispose).toHaveBeenCalledTimes(1);
    expect(secondTerm.wasmTerm.free).not.toHaveBeenCalled();
  });
});
