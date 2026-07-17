import type React from "react";
import { useCallback, useEffect, useState } from "react";

// Slack-style composer preference. When ON (the default, matching the Claude
// desktop app), plain Enter sends the message and Shift+Enter inserts a newline;
// when OFF, the composer reverts to Enter = newline and ⌘/Ctrl+Enter sends.
//
// This is a personal, per-browser UI preference (not server state), so it lives
// in localStorage exactly like the theme preference. Both consumers — the
// composer and the settings toggle — subscribe to the same key and stay in sync
// live via a custom event (same tab) and the native `storage` event (other tabs).
const STORAGE_KEY = "engrams-enter-to-send";
const CHANGE_EVENT = "engrams-enter-to-send-change";
const DEFAULT_ENABLED = true;

function read(): boolean {
  if (typeof localStorage === "undefined") return DEFAULT_ENABLED;
  const stored = localStorage.getItem(STORAGE_KEY);
  // Only an explicit opt-out turns it off; absence means the default.
  return stored === null ? DEFAULT_ENABLED : stored === "true";
}

/** Returns true if this keyboard event should submit the composer. */
export function isSubmitKey(e: React.KeyboardEvent, enterToSend: boolean): boolean {
  const plainEnter =
    e.key === "Enter" &&
    !e.shiftKey &&
    !e.metaKey &&
    !e.ctrlKey &&
    !e.altKey &&
    !e.nativeEvent.isComposing;
  return (e.key === "Enter" && (e.metaKey || e.ctrlKey)) || (enterToSend && plainEnter);
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
