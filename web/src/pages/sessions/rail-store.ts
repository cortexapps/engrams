import { create } from "zustand";

import type { RailBand } from "./useRailSessions";

// The rail's search lives in a store because useRailSessions has three
// consumers — the rail, the jump keymap, and the command menu — that must issue
// the same ListTasks query key and share React Query's cache (ADR 0087).
// MySessions and AllSessions use the same parameter-keyed pagination model for
// their own filters. Loaded-page depth is query-cache state, not Zustand state.

/** Which bands are open. Finished starts closed: it is the biggest band on any
 *  account that has been used for a while, and it is history — the rail is a
 *  switcher for work in flight. */
const DEFAULT_OPEN: Record<RailBand, boolean> = {
  attention: true,
  working: true,
  idle: true,
  finished: false,
};

const BANDS_STORAGE_KEY = "engrams.railBands";

// Spread OVER the defaults, never replace them: a band added after this was
// written must arrive at its own default, not `undefined` (which reads as
// closed and would hide live work). A corrupt or absent value falls back whole.
function readOpenBands(): Record<RailBand, boolean> {
  try {
    return { ...DEFAULT_OPEN, ...JSON.parse(localStorage.getItem(BANDS_STORAGE_KEY) ?? "{}") };
  } catch {
    return DEFAULT_OPEN;
  }
}

interface RailState {
  search: string;
  setSearch: (search: string) => void;
  /** Per-band disclosure, remembered per browser. */
  openBands: Record<RailBand, boolean>;
  setBandOpen: (band: RailBand, open: boolean) => void;
}

export const useRailStore = create<RailState>((set) => ({
  search: "",
  setSearch: (search) => set({ search }),
  openBands: readOpenBands(),
  setBandOpen: (band, open) =>
    set((state) => {
      const openBands = { ...state.openBands, [band]: open };
      try {
        localStorage.setItem(BANDS_STORAGE_KEY, JSON.stringify(openBands));
      } catch {
        /* private-mode / quota — keep the in-memory choice */
      }
      return { openBands };
    }),
}));
