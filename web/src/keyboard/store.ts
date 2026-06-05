import { create } from 'zustand';

// One small store for the keyboard layer's transient UI state. The command
// palette, the global New Session dialog, and the shortcuts cheatsheet are all
// openable from several places (a key, a palette row, a rail button), so their
// open-state lives here rather than in any one component. `jumpHeld` mirrors
// whether the ⌥/Alt session-jump modifier is currently down, so the sessions
// rail can reveal its 1–9 jump numbers while it's held.
interface KeyboardUiState {
  paletteOpen: boolean;
  newSessionOpen: boolean;
  shortcutsOpen: boolean;
  jumpHeld: boolean;

  setPaletteOpen: (open: boolean) => void;
  togglePalette: () => void;
  setNewSessionOpen: (open: boolean) => void;
  openNewSession: () => void;
  setShortcutsOpen: (open: boolean) => void;
  setJumpHeld: (held: boolean) => void;
}

export const useKeyboardUi = create<KeyboardUiState>((set) => ({
  paletteOpen: false,
  newSessionOpen: false,
  shortcutsOpen: false,
  jumpHeld: false,

  setPaletteOpen: (open) => set({ paletteOpen: open }),
  togglePalette: () => set((s) => ({ paletteOpen: !s.paletteOpen })),
  setNewSessionOpen: (open) => set({ newSessionOpen: open }),
  // Opening New Session from a key/palette also closes the palette so focus
  // lands cleanly in the dialog (two stacked modals fight over the focus trap).
  openNewSession: () => set({ newSessionOpen: true, paletteOpen: false }),
  setShortcutsOpen: (open) => set({ shortcutsOpen: open }),
  setJumpHeld: (held) => set((s) => (s.jumpHeld === held ? s : { jumpHeld: held })),
}));

/** True when any keyboard-owned modal is up — single-key accelerators and the
 * jump layer disable themselves so they don't fire underneath an open dialog. */
export function useAnyKeyboardModalOpen(): boolean {
  return useKeyboardUi(
    (s) => s.paletteOpen || s.newSessionOpen || s.shortcutsOpen,
  );
}
