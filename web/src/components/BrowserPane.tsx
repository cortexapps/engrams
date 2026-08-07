import { useEffect, useRef, useState } from "react";

import { API_BASE } from "../lib/base";
import { PaneStatus } from "./PaneStatus";

// In-browser BROWSER tab (ADR 0065). Lazy-loads `@novnc/novnc` (the RFB
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
  // Bumping this disconnects the current RFB (effect cleanup) and re-runs the
  // mount — a real reconnect, driven by the Reconnect button on a dropped
  // connection.
  const [reconnectKey, setReconnectKey] = useState(0);

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
          setErrorMessage(`Failed to load browser viewer: ${e}`);
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
        rfb.resizeSession = true; // ADR 0065: resolution tracks the panel via RANDR
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
  }, [sessionId, reconnectKey]);

  const caption = status === "loading" ? "loading browser viewer…" : "launching browser…";
  const message =
    status === "error"
      ? (errorMessage ?? "browser unavailable")
      : `browser connection closed${errorMessage ? ` — ${errorMessage}` : ""}`;

  return (
    <section className="flex h-full min-h-0 flex-col">
      <div className="relative min-h-0 flex-1 overflow-hidden">
        <div ref={containerRef} className="absolute inset-0" />
        <PaneStatus
          phase={status}
          caption={caption}
          message={message}
          onReconnect={() => {
            setErrorMessage(null);
            setStatus("loading");
            setReconnectKey((k) => k + 1);
          }}
        />
      </div>
    </section>
  );
}
