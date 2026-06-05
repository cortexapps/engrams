import { useEffect, useRef } from "react";
import { useNavigate } from "@tanstack/react-router";
import { useRailSessions } from "../pages/sessions/useRailSessions";
import { useKeyboardUi, useAnyKeyboardModalOpen } from "./store";

// The ⌥/Alt "session-jump layer". Three behaviours on one window listener,
// because they share the same modifier and one of them (hold-to-reveal) isn't
// a discrete chord react-hotkeys-hook can express:
//
//   • Hold ⌥            → the sessions rail reveals its 1–9 jump numbers.
//   • ⌥1 … ⌥9          → jump to the Nth rail session.
//   • ⌥[ / ⌥]          → previous / next rail session (wraps).
//
// We match on event.code (Digit1, BracketLeft, …) so it's keyboard-layout proof
// and survives macOS turning ⌥1 into "¡": preventDefault on those combos stops
// that character ever reaching a focused composer, which is exactly why these
// work while typing. Bare ⌥ is never preventDefaulted — it has no default
// action inside a textarea — so normal Option-key text editing is untouched.
//
// We deliberately avoid ⌘/Ctrl+digit (the browser owns those for tab switching
// and won't let us cancel them) and Alt+Arrow (Windows back/forward).

export function useSessionJumpKeys(): void {
  const navigate = useNavigate();
  const { rows, openId } = useRailSessions();
  const setJumpHeld = useKeyboardUi((s) => s.setJumpHeld);
  const blocked = useAnyKeyboardModalOpen();

  // Keep the listener stable (rows refetch every 1s; we don't want to rebind
  // the window listener that often) by reading live values from a ref.
  const live = useRef({ rows, openId, blocked, navigate, setJumpHeld });
  live.current = { rows, openId, blocked, navigate, setJumpHeld };

  useEffect(() => {
    const go = (id: string) => live.current.navigate({ to: "/sessions/$id", params: { id } });

    const onKeyDown = (e: KeyboardEvent) => {
      const { rows, openId, blocked } = live.current;
      // Track the held modifier for the rail reveal. Only a "clean" Alt (no
      // ⌘/Ctrl/⇧) arms the layer, so ⌥⇧3-style chords don't light it up.
      if (e.altKey && !e.metaKey && !e.ctrlKey && !e.shiftKey) {
        if (!blocked) live.current.setJumpHeld(true);
      } else if (e.key === "Alt") {
        // Alt pressed together with another modifier — not our layer.
        live.current.setJumpHeld(false);
      }

      if (blocked || !e.altKey || e.metaKey || e.ctrlKey || e.shiftKey) return;
      if (rows.length === 0) return;

      const code = e.code;
      if (code.startsWith("Digit")) {
        const n = Number(code.slice(5));
        if (n >= 1 && n <= 9 && n <= rows.length) {
          e.preventDefault();
          go(rows[n - 1].id);
        }
        return;
      }
      if (code === "BracketLeft" || code === "BracketRight") {
        e.preventDefault();
        const i = openId ? rows.findIndex((r) => r.id === openId) : -1;
        const len = rows.length;
        const next =
          code === "BracketRight"
            ? i < 0
              ? 0
              : (i + 1) % len
            : i < 0
              ? len - 1
              : (i - 1 + len) % len;
        go(rows[next].id);
      }
    };

    const onKeyUp = (e: KeyboardEvent) => {
      if (e.key === "Alt" || !e.altKey) live.current.setJumpHeld(false);
    };
    // Alt-Tabbing away or losing focus mid-hold must not leave the rail stuck
    // showing numbers.
    const clear = () => live.current.setJumpHeld(false);

    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
    window.addEventListener("blur", clear);
    document.addEventListener("visibilitychange", clear);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
      window.removeEventListener("blur", clear);
      document.removeEventListener("visibilitychange", clear);
    };
  }, []);
}
