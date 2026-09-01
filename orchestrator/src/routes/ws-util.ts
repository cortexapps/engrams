/**
 * Shared helpers for raw WebSocket upgrade handlers (the UpgradeHook
 * shape server.ts dispatches — see spec-sync.ts for the first one).
 *
 * THE ACCEPT-FIRST INVARIANT (Bun 1.4.0, the #1333 outage class):
 * under Bun, `ws` is the runtime's builtin shim and its handshake
 * completion delegates to native `server.upgrade(req)`, which is only
 * valid inside the request's OWN event-loop turn. Any awaited I/O — a
 * DB auth lookup, even a `setTimeout(0)` macrotask — before
 * `handleUpgrade` invalidates the window: native upgrade returns
 * false, the shim's abort path throws, and the client sees a dead
 * socket (the load balancer logs websocket_handshake_failed).
 *
 * So every handler here accepts FIRST, authorizes SECOND, and closes
 * with a 4000+status code on refusal. An unauthorized caller costs one
 * accepted-then-closed socket — the same thing an explicit rejection
 * costs. shell.ts and vnc.ts learned this the hard way: they kept the
 * Hono-middleware auth-then-upgrade shape after #1333 migrated the
 * other three routes, and were broken on every deployment for 11 days
 * before a fresh-deployment walk opened the Shell tab.
 */

import type { IncomingMessage } from "node:http";
import type { Socket } from "node:net";
import type { WebSocket, WebSocketServer } from "ws";

/** The raw Node request's headers as a fetch `Headers` (what the
 * header guards consume). Joins repeated headers with ", ". */
export function requestHeaders(req: IncomingMessage): Headers {
  const headers = new Headers();
  for (const [key, value] of Object.entries(req.headers)) {
    if (value === undefined) continue;
    headers.set(key, Array.isArray(value) ? value.join(", ") : value);
  }
  return headers;
}

/** Complete the handshake, then immediately close with a status-coded
 * frame (4000+status, capped at 4999) — the refusal a browser can
 * actually read, unlike a destroyed socket. */
export function rejectUpgrade(
  wss: WebSocketServer,
  req: IncomingMessage,
  socket: Socket,
  head: Buffer,
  status: number,
  label: string,
): true {
  try {
    wss.handleUpgrade(req, socket, head, (ws) => {
      ws.close(Math.min(4000 + status, 4999), `${label} ${status}`);
    });
  } catch {
    socket.destroy();
  }
  return true;
}

/** Accept the handshake synchronously (the invariant above). Returns
 * the socket, or null if the handshake could not complete — in which
 * case the raw socket has been destroyed. */
export function acceptUpgrade(
  wss: WebSocketServer,
  req: IncomingMessage,
  socket: Socket,
  head: Buffer,
): WebSocket | null {
  let accepted: WebSocket | null = null;
  try {
    wss.handleUpgrade(req, socket, head, (ws) => {
      accepted = ws;
    });
  } catch {
    socket.destroy();
    return null;
  }
  if (!accepted) socket.destroy();
  return accepted;
}
