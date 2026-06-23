import { create } from "zustand";

// One small store for the keyboard layer's transient UI state. The command
// palette and the shortcuts cheatsheet are openable from several places (a key,
// a palette row), so their open-state lives here rather than in any one
// component. `jumpHeld` mirrors whether the ⌥/Alt session-jump modifier is
// currently down, so the sessions rail can reveal its 1–9 jump numbers while
// it's held. `composerFocusNonce` is the "start a task" signal: bumping it asks
// the start screen's composer to take focus — the `c` key and the ⌘K "start
// task" row navigate to /sessions and bump it, so the cursor lands in the
// composer whether the page was already mounted or not.
interface KeyboardUiState {
  paletteOpen: boolean;
  shortcutsOpen: boolean;
  jumpHeld: boolean;
  composerFocusNonce: number;

  setPaletteOpen: (open: boolean) => void;
  togglePalette: () => void;
  setShortcutsOpen: (open: boolean) => void;
  setJumpHeld: (held: boolean) => void;
  /** Ask the start-screen composer to focus (after navigating to it). */
  requestComposerFocus: () => void;
}

export const useKeyboardUi = create<KeyboardUiState>((set) => ({
  paletteOpen: false,
  shortcutsOpen: false,
  jumpHeld: false,
  composerFocusNonce: 0,

  setPaletteOpen: (open) => set({ paletteOpen: open }),
  togglePalette: () => set((s) => ({ paletteOpen: !s.paletteOpen })),
  setShortcutsOpen: (open) => set({ shortcutsOpen: open }),
  setJumpHeld: (held) => set((s) => (s.jumpHeld === held ? s : { jumpHeld: held })),
  // Closing the palette here too: the ⌘K row navigates + focuses, and a lingering
  // palette would steal the focus the composer is about to take.
  requestComposerFocus: () =>
    set((s) => ({ composerFocusNonce: s.composerFocusNonce + 1, paletteOpen: false })),
}));

/** True when any keyboard-owned modal is up — single-key accelerators and the
 * jump layer disable themselves so they don't fire underneath an open dialog. */
export function useAnyKeyboardModalOpen(): boolean {
  return useKeyboardUi((s) => s.paletteOpen || s.shortcutsOpen);
}
