import { create } from "zustand";

// The rail's filter state (search + visible-row budget) lives in a store, not
// in the rail component, because useRailSessions has three consumers — the
// rail, the ⌥-jump keymap, and the command palette — and all three must issue
// the SAME ListTasks query to share React Query's cache and keep the visible
// rows and the jump targets in lockstep (ADR 0087). Reaching the rail's
// infinite-scroll sentinel grows the budget; typing resets it so a new search
// starts back at the first page-size step.

export const INITIAL_LIMIT = 25;
export const STEP = 25;

interface RailState {
  search: string;
  limit: number;
  setSearch: (search: string) => void;
  showMore: () => void;
}

export const useRailStore = create<RailState>((set) => ({
  search: "",
  limit: INITIAL_LIMIT,
  setSearch: (search) => set({ search, limit: INITIAL_LIMIT }),
  showMore: () => set((state) => ({ limit: state.limit + STEP })),
}));
