import { create } from "zustand";

// The rail's search lives in a store because useRailSessions has three
// consumers — the rail, the jump keymap, and the command menu — that must issue
// the same ListTasks query key and share React Query's cache (ADR 0087).
// MySessions and AllSessions use the same parameter-keyed pagination model for
// their own filters. Loaded-page depth is query-cache state, not Zustand state.

interface RailState {
  search: string;
  setSearch: (search: string) => void;
}

export const useRailStore = create<RailState>((set) => ({
  search: "",
  setSearch: (search) => set({ search }),
}));
