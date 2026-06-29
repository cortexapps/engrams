import { useEffect, useRef, useState } from "react";

import { API_BASE } from "../lib/base";

// In-browser BROWSER tab (ADR 0064). Lazy-loads `@novnc/novnc` (the RFB
// client is the heavy bit) on first mount, opens a WebSocket to
// `/sessions/:id/vnc`, and drives the in-guest Xvfb desktop over raw RFB.
// The orchestrator relay does websockify's job (raw RFB bytes over the
// auth-gated WebSocket — no protocol translation), so RFB connects to the
// WS URL directly, exactly as TerminalPane's shell relay does.
//
// Mounted once and kept across tab switches by the parent (display:none),
// like TerminalPane — the cleanup below disconnects + clears the container
// only on real unmount, and a fresh mount re-opens the session.

export interface BrowserPaneProps {
  sessionId: string;
}

type Status = "loading" | "connecting" | "connected" | "closed" | "error";

// noVNC 1.7's `exports` map points the bare specifier at `core/rfb.js`,
// whose default export is the `RFB` class. (There is no `lib/` dir.) Memoize
// the dynamic import so React StrictMode's double-effect in dev awaits one
// module load rather than racing two.
let rfbModulePromise: Promise<typeof import("@novnc/novnc")> | null = null;
function loadRfb(): Promise<typeof import("@novnc/novnc")> {
  if (!rfbModulePromise) {
    rfbModulePromise = import("@novnc/novnc");
  }
  return rfbModulePromise;
}

export function BrowserPane({ sessionId }: BrowserPaneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<Status>("loading");
  const [errorMessage, setErrorMessage] = useState<string | null>(null);

  useEffect(() => {
    if (!containerRef.current) return;
    const container = containerRef.current;
    let disposed = false;
    let rfb: import("@novnc/novnc").default | null = null;

    (async () => {
      let mod: typeof import("@novnc/novnc");
      try {
        mod = await loadRfb();
      } catch (e) {
        if (!disposed) {
          setStatus("error");
          setErrorMessage(`failed to load browser viewer: ${e}`);
        }
        return;
      }
      if (disposed) return;

      // StrictMode's first-effect cleanup may have left noVNC's canvas in
      // the container; clear it before opening a new RFB so we don't stack
      // two viewers (same defensive clear as TerminalPane).
      while (container.firstChild) {
        container.removeChild(container.firstChild);
      }

      setStatus("connecting");
      const wsUrl = `${
        location.protocol === "https:" ? "wss:" : "ws:"
      }//${location.host}${API_BASE}/sessions/${encodeURIComponent(sessionId)}/vnc`;

      try {
        rfb = new mod.default(container, wsUrl, { wsProtocols: [] });
        rfb.viewOnly = false; // the human DRIVES the browser
        rfb.scaleViewport = true;
        rfb.resizeSession = true; // ADR 0064: resolution tracks the panel via RANDR
        rfb.addEventListener("connect", () => {
          if (!disposed) setStatus("connected");
        });
        rfb.addEventListener("disconnect", (e) => {
          if (disposed) return;
          setStatus("closed");
          // RFB's `disconnect` event detail carries `{ clean: boolean }`.
          const detail = (e as CustomEvent<{ clean?: boolean }>).detail;
          if (detail && detail.clean === false) {
            setErrorMessage("browser connection closed unexpectedly");
          }
        });
      } catch (err) {
        if (!disposed) {
          setStatus("error");
          setErrorMessage(String(err));
        }
      }
    })();

    return () => {
      disposed = true;
      try {
        rfb?.disconnect();
      } catch {
        // ignore
      }
      // Clear DOM noVNC left behind. React only unmounts the wrapping <div>
      // when the parent unmounts; tab switches inside SessionDetail keep the
      // parent alive, so we'd otherwise stack canvases on each toggle.
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
          loading browser viewer…
        </p>
      )}
      {status === "connecting" && (
        <p className="mb-3 font-display text-[0.92rem] text-muted-foreground italic">
          launching browser…
        </p>
      )}
      {status === "closed" && (
        <p className="mb-3 font-display text-[0.92rem] text-muted-foreground italic">
          browser connection closed{errorMessage ? ` — ${errorMessage}` : ""} — switch tabs and back
          to relaunch.
        </p>
      )}
      {status === "error" && (
        <p className="mb-3 font-mono text-[0.78rem] text-destructive">
          {errorMessage ?? "browser unavailable"}
        </p>
      )}
      <div
        ref={containerRef}
        className="relative min-h-0 flex-1 overflow-hidden rounded-md border bg-card"
      />
    </section>
  );
}
