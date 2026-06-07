import { useEffect, useRef, useState } from "react";

import { API_BASE } from "../api";

// In-browser shell tab. Lazy-loads `ghostty-web` (~400 KB WASM) on
// first mount, opens a WebSocket to `/sessions/:id/shell`, and bridges
// the connection via ttyd's text-prefix protocol (which the
// coordinator does NOT translate — it's a dumb byte relay).
//
// ttyd protocol (mirrored from
// https://github.com/tsl0922/ttyd/blob/main/src/protocol.h):
//
// Both directions use a single-byte command discriminator at offset 0
// followed by the payload. The protocol is symmetric in shape but the
// command set differs by direction.
//
//   client → server (text frame):
//     "0" + raw user input
//     "1" + JSON{columns, rows}              resize
//   client → server (first frame, text JSON):
//     JSON{AuthToken: "", columns, rows}
//   server → client (binary frame):
//     0x30 ('0') + raw bytes                 terminal output
//     0x31 ('1') + UTF-8 JSON                set window title
//     0x32 ('2') + UTF-8 JSON                preferences
//   server → client (text frame, legacy):
//     same byte-prefix shape, just on a text frame instead of binary
//
// We render only the OUTPUT payload; title and preferences are dropped.

export interface TerminalPaneProps {
  sessionId: string;
}

const ttyClient = {
  INPUT: "0",
  RESIZE: "1",
} as const;

// Server-side discriminator bytes (ASCII '0'/'1'/'2' = 0x30/0x31/0x32).
const SERVER_OUTPUT = 0x30;
const SERVER_TITLE = 0x31;
const SERVER_PREFERENCES = 0x32;

// The terminal base (surface, ink, cursor, selection) tracks the
// Aston-racing theme so the shell reads as part of the page, not a
// pasted-in cream box. Cursor is the theme's "live" accent: racing green
// on the celadon paper (lime would be invisible on paper — same reason
// --ring is green in light mode), lime on the green-black ground.
//
// The ANSI 16 are program colors (`ls --color`, `grep`, prompts), not
// brand tokens — kept legible against both grounds. xterm needs literal
// hex, so these are resolved here rather than from CSS vars; a theme
// toggle while the shell is open re-colors on the next mount.
const ANSI = {
  black: "#1c2b29",
  red: "#d9544d",
  green: "#7faa3f",
  yellow: "#c7a83c",
  blue: "#4f93c9",
  magenta: "#c56aa6",
  cyan: "#3fa79b",
  white: "#cdd9c4",
  brightBlack: "#5a6b63",
  brightRed: "#e8736b",
  brightGreen: "#9bc457",
  brightYellow: "#dcc24f",
  brightBlue: "#6fb0e0",
  brightMagenta: "#d98cc0",
  brightCyan: "#5fc4b6",
  brightWhite: "#eef3e8",
};

const LIGHT_THEME = {
  background: "#eef3e7", // lab-paper sheet (--background, light)
  foreground: "#223133", // petrol ink (--foreground, light)
  cursor: "#5f8f3a", // racing green — visible on paper
  cursorAccent: "#eef3e7",
  selectionBackground: "#dae7c8",
  ...ANSI,
};

const DARK_THEME = {
  background: "#11201d", // racing green-black (--background, dark)
  foreground: "#d7e4cf", // pale sage ink
  cursor: "#b6e84a", // Aston-F1 lime — pops on the dark ground
  cursorAccent: "#11201d",
  selectionBackground: "#21332f",
  ...ANSI,
};

function pickTheme() {
  const dark =
    typeof document !== "undefined" && document.documentElement.classList.contains("dark");
  return dark ? DARK_THEME : LIGHT_THEME;
}

// Module-level memoization of ghostty-web's WASM init. React's
// StrictMode runs effects twice in dev; without this, we'd call
// `mod.init()` twice in quick succession against the same global
// WASM module — second call can throw or hang. With memoization,
// the first effect kicks off the load and the second awaits the
// same promise.
let ghosttyModulePromise: Promise<typeof import("ghostty-web")> | null = null;
function loadGhostty(): Promise<typeof import("ghostty-web")> {
  if (!ghosttyModulePromise) {
    ghosttyModulePromise = import("ghostty-web").then(async (mod) => {
      await mod.init();
      return mod;
    });
  }
  return ghosttyModulePromise;
}

export function TerminalPane({ sessionId }: TerminalPaneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<"loading" | "connecting" | "connected" | "closed" | "error">(
    "loading",
  );
  const [errorMessage, setErrorMessage] = useState<string | null>(null);

  useEffect(() => {
    if (!containerRef.current) return;
    const container = containerRef.current;
    let disposed = false;
    let term: import("ghostty-web").Terminal | null = null;
    let fitAddon: import("ghostty-web").FitAddon | null = null;
    let ws: WebSocket | null = null;

    (async () => {
      let mod: typeof import("ghostty-web");
      try {
        mod = await loadGhostty();
      } catch (e) {
        if (!disposed) {
          setStatus("error");
          setErrorMessage(`failed to load terminal renderer: ${e}`);
        }
        return;
      }
      if (disposed) return;

      // Defensive: StrictMode's first-effect cleanup may have left a
      // canvas behind in the container if `term.dispose()` didn't
      // unmount it. Clear the container before opening the new
      // Terminal so we don't render two stacked canvases.
      while (container.firstChild) {
        container.removeChild(container.firstChild);
      }

      term = new mod.Terminal({
        fontSize: 13,
        fontFamily:
          '"JetBrains Mono Variable", "Berkeley Mono", "SF Mono", ui-monospace, monospace',
        theme: pickTheme(),
        cursorBlink: true,
        scrollback: 5000,
      });

      fitAddon = new mod.FitAddon();
      term.loadAddon(fitAddon);
      term.open(container);
      try {
        fitAddon.fit();
        fitAddon.observeResize();
      } catch {
        // ignore — fit is best-effort
      }
      // ghostty-web's WASM allocator can hand a freshly-created
      // Terminal a cell grid that overlaps a previous mount's leaked
      // grid memory, so the renderer paints stale glyphs from the
      // prior bash session. Walk + zero every cell at the *final*
      // post-fit grid size: \x1b[2J = clear viewport, \x1b[3J = clear
      // scrollback, \x1b[H = cursor home. Writing this *before* fit()
      // (an earlier shape of this code) only clears the pre-resize
      // grid; the cells that fit() adds come back stale.
      try {
        term.write("\x1b[2J\x1b[3J\x1b[H");
      } catch {
        // ignore
      }

      setStatus("connecting");
      const wsUrl = `${
        location.protocol === "https:" ? "wss:" : "ws:"
      }//${location.host}${API_BASE}/sessions/${encodeURIComponent(sessionId)}/shell`;
      const localWs = new WebSocket(wsUrl, "tty");
      ws = localWs;
      localWs.binaryType = "arraybuffer";

      localWs.onopen = () => {
        if (disposed) return;
        const cols = term?.cols ?? 80;
        const rows = term?.rows ?? 24;
        try {
          localWs.send(JSON.stringify({ AuthToken: "", columns: cols, rows }));
        } catch {
          // ignore — close handler will surface the failure
        }
        setStatus("connected");
      };

      localWs.onmessage = (e) => {
        if (!term) return;
        // ttyd sends a single discriminator byte followed by the
        // payload. Route purely on that byte regardless of whether
        // the frame arrived as binary (modern ttyd) or text (legacy).
        // Earlier we routed on frame opcode and wrote binary frames
        // whole — that's why every keystroke surfaced as `0X` in the
        // terminal: the prefix `0x30` was being rendered.
        let bytes: Uint8Array;
        if (typeof e.data === "string") {
          bytes = new TextEncoder().encode(e.data);
        } else if (e.data instanceof ArrayBuffer) {
          bytes = new Uint8Array(e.data);
        } else {
          return;
        }
        if (bytes.length === 0) return;
        const cmd = bytes[0];
        const body = bytes.subarray(1);
        switch (cmd) {
          case SERVER_OUTPUT:
            term.write(body);
            break;
          case SERVER_TITLE:
          case SERVER_PREFERENCES:
            // Title and preferences are deliberately dropped — the
            // dashboard owns the page chrome and we don't honor
            // server-side prefs.
            break;
          default:
            // Unknown command — drop silently rather than corrupt
            // the buffer with a stray prefix byte.
            break;
        }
      };

      localWs.onerror = () => {
        if (!disposed) {
          setStatus("error");
          setErrorMessage("shell connection error");
        }
      };
      localWs.onclose = (ev) => {
        // The cleanup function calls `localWs.close()` deliberately;
        // when that happens, `disposed` is already true and we skip
        // the UI update. Any unsolicited close (server-side hangup,
        // proxy error) flows through and surfaces with the close
        // code so we can see *why* it died, not just "it died".
        if (disposed) return;
        const reason =
          ev.reason ||
          (ev.code === 1006 ? "abnormal close (no close frame received)" : `code ${ev.code}`);
        setStatus("closed");
        setErrorMessage(reason);
      };

      term.onData((data) => {
        if (localWs.readyState === WebSocket.OPEN) {
          localWs.send(ttyClient.INPUT + data);
        }
      });
      term.onResize(({ cols, rows }) => {
        if (localWs.readyState === WebSocket.OPEN) {
          localWs.send(ttyClient.RESIZE + JSON.stringify({ columns: cols, rows }));
        }
      });
    })();

    return () => {
      disposed = true;
      try {
        ws?.close();
      } catch {
        // ignore
      }
      try {
        fitAddon?.dispose?.();
      } catch {
        // ignore
      }
      // Terminal.dispose() runs cleanupComponents() which already frees
      // the underlying wasmTerm. Calling wasmTerm.free() ourselves first
      // double-frees the WASM allocation: small writes survive on the
      // *next* mount (e.g. `echo X`), but a larger one like the
      // colorized output of `ls /` hits the corrupted region and throws
      // `RuntimeError: memory access out of bounds` in term.write,
      // freezing the canvas while the websocket keeps flowing.
      try {
        term?.dispose();
      } catch {
        // ignore
      }
      // Clear any DOM left behind by the renderer. ghostty-web's
      // dispose() doesn't always detach its canvas, and React only
      // unmounts the wrapping <div> when the *parent* unmounts —
      // tab switches inside SessionDetail keep the parent alive,
      // so we'd otherwise stack canvases on each toggle.
      try {
        while (container.firstChild) {
          container.removeChild(container.firstChild);
        }
      } catch {
        // ignore
      }
    };
  }, [sessionId]);

  return (
    <section className="flex h-full flex-col px-6 py-4">
      {status === "loading" && (
        <p className="mb-3 font-display text-[0.92rem] text-muted-foreground italic">
          loading terminal renderer…
        </p>
      )}
      {status === "connecting" && (
        <p className="mb-3 font-display text-[0.92rem] text-muted-foreground italic">
          opening shell…
        </p>
      )}
      {status === "closed" && (
        <p className="mb-3 font-display text-[0.92rem] text-muted-foreground italic">
          shell connection closed{errorMessage ? ` — ${errorMessage}` : ""} — switch tabs and back
          to reconnect.
        </p>
      )}
      {status === "error" && (
        <p className="mb-3 font-mono text-[0.78rem] text-destructive">
          {errorMessage ?? "shell unavailable"}
        </p>
      )}
      <div ref={containerRef} className="min-h-0 flex-1 rounded-md border bg-card px-3 py-2" />
    </section>
  );
}
