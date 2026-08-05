import { useEffect, useRef, useState } from "react";

import { API_BASE } from "../lib/base";
import { PaneStatus } from "./PaneStatus";

// In-browser IDE tab (ADR 0085). Unlike the Shell/Browser panes there is no
// client library and no socket for this component to own directly — the
// orchestrator's `/api/v1/sessions/:id/ide/*` proxy serves code-server's own
// web app (HTTP + its own WS handshake) same-origin, so the whole client is
// an iframe.
//
// The one wrinkle: an iframe's `onError` doesn't fire for a same-origin app
// that loads but 5xxs, and code-server's launcher (`engram-browser`-shaped,
// `--ensure` under flock) can take a moment to come up on first ensure. So we
// pre-flight `GET .../ide/healthz` before ever mounting the iframe, polling
// with backoff while the guest-side server isn't answering yet ("starting
// IDE…" — the same calm tone as the Browser tab's "launching browser…"), and
// only swap in the iframe once the health check succeeds. The iframe's own
// `onLoad` then covers the (rare) remaining gap between "proxy healthy" and
// "code-server's client JS finished loading".

export interface IdePaneProps {
  sessionId: string;
}

type Status = "loading" | "connecting" | "connected" | "error";

const HEALTH_BACKOFF_INITIAL_MS = 500;
const HEALTH_BACKOFF_MAX_MS = 5_000;
// Past this many failed probes we stop polling silently and surface a
// Reconnect affordance — a wedged launcher (e.g. the `ide` skill missing from
// the baked image, ADR 0085 §Consequences) should say so rather than spin
// forever.
const HEALTH_MAX_ATTEMPTS = 12;

function healthUrl(sessionId: string): string {
  return `${API_BASE}/sessions/${encodeURIComponent(sessionId)}/ide/healthz`;
}

function iframeUrl(sessionId: string): string {
  return `${API_BASE}/sessions/${encodeURIComponent(sessionId)}/ide/`;
}

export function IdePane({ sessionId }: IdePaneProps) {
  const [status, setStatus] = useState<Status>("loading");
  const [errorMessage, setErrorMessage] = useState<string | null>(null);
  // Once the health probe succeeds we mount the iframe and never unmount it
  // for this pane instance (short of a real Reconnect) — remounting would
  // drop code-server's live editor state and websockets.
  const [ideReady, setIdeReady] = useState(false);
  // Bumping this restarts the probe loop and remounts the iframe — a real
  // reconnect, driven by the Reconnect button, mirroring TerminalPane/
  // BrowserPane.
  const [reconnectKey, setReconnectKey] = useState(0);
  const iframeRef = useRef<HTMLIFrameElement>(null);

  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | null = null;

    setIdeReady(false);
    setStatus("loading");
    setErrorMessage(null);

    async function probe(attempt: number) {
      if (disposed) return;
      try {
        const res = await fetch(healthUrl(sessionId), { credentials: "include" });
        if (disposed) return;
        if (res.ok) {
          setIdeReady(true);
          setStatus("connecting"); // iframe is about to mount; its onLoad flips us to connected
          return;
        }
        throw new Error(`healthz returned ${res.status}`);
      } catch (e) {
        if (disposed) return;
        if (attempt + 1 >= HEALTH_MAX_ATTEMPTS) {
          setStatus("error");
          setErrorMessage(`IDE did not become ready: ${e}`);
          return;
        }
        const delay = Math.min(HEALTH_BACKOFF_INITIAL_MS * 2 ** attempt, HEALTH_BACKOFF_MAX_MS);
        timer = setTimeout(() => probe(attempt + 1), delay);
      }
    }

    void probe(0);

    return () => {
      disposed = true;
      if (timer) clearTimeout(timer);
    };
  }, [sessionId, reconnectKey]);

  const caption = status === "loading" ? "starting IDE…" : "loading IDE…";
  const message = errorMessage ?? "IDE unavailable";

  return (
    <section className="flex h-full min-h-0 flex-col">
      <div className="relative min-h-0 flex-1 overflow-hidden">
        {ideReady && (
          <iframe
            ref={iframeRef}
            src={iframeUrl(sessionId)}
            title="IDE"
            className="absolute inset-0 h-full w-full border-0"
            allow="clipboard-read; clipboard-write"
            onLoad={() => setStatus("connected")}
            onError={() => {
              setStatus("error");
              setErrorMessage("IDE failed to load");
            }}
          />
        )}
        <PaneStatus
          phase={status}
          caption={caption}
          message={message}
          onReconnect={
            status === "error"
              ? () => {
                  setErrorMessage(null);
                  setReconnectKey((k) => k + 1);
                }
              : undefined
          }
        />
      </div>
    </section>
  );
}
