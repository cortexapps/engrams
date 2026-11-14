import { log } from "../log.ts";

// Notifications request a recheck; committed authority rows remain the source of truth.
const listeners = new Set<() => void>();
export function observeMcpAuthorityChanges(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}
export function notifyMcpAuthorityChanged(): void {
  for (const listener of listeners) {
    try {
      listener();
    } catch {
      log.warn("MCP authority notification unavailable");
    }
  }
}
