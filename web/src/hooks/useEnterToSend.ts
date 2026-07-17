import { useCallback, useEffect, useState } from "react";

// Slack-style composer preference. When ON, plain Enter sends the message and
// Shift+Enter inserts a newline; when OFF (the default), the composer keeps its
// writing-surface behaviour — Enter is a newline and ⌘/Ctrl+Enter sends.
//
// This is a personal, per-browser UI preference (not server state), so it lives
// in localStorage exactly like the theme preference. Both consumers — the
// composer and the settings toggle — subscribe to the same key and stay in sync
// live via a custom event (same tab) and the native `storage` event (other tabs).
const STORAGE_KEY = "engrams-enter-to-send";
const CHANGE_EVENT = "engrams-enter-to-send-change";

function read(): boolean {
  if (typeof localStorage === "undefined") return false;
  return localStorage.getItem(STORAGE_KEY) === "true";
}

/** Reactive accessor for the Enter-to-send preference: `[enabled, setEnabled]`. */
export function useEnterToSend(): [boolean, (value: boolean) => void] {
  const [enabled, setEnabled] = useState<boolean>(read);

  useEffect(() => {
    const sync = () => setEnabled(read());
    // `storage` fires only for OTHER tabs; the custom event covers same-tab
    // mounts (composer + settings toggle flipping together without a reload).
    window.addEventListener("storage", sync);
    window.addEventListener(CHANGE_EVENT, sync);
    return () => {
      window.removeEventListener("storage", sync);
      window.removeEventListener(CHANGE_EVENT, sync);
    };
  }, []);

  const set = useCallback((value: boolean) => {
    try {
      localStorage.setItem(STORAGE_KEY, String(value));
    } catch {
      /* private-mode / quota — keep the in-memory value */
    }
    setEnabled(value);
    window.dispatchEvent(new Event(CHANGE_EVENT));
  }, []);

  return [enabled, set];
}
