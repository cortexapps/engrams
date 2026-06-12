import { createContext, useContext } from "react";
import type { SessionState } from "../../lib/types";

// Carries the session lifecycle state into the Thread subtree so the composer
// can show the right banner/hint (terminal states block sending; idle/starting
// states explain what happens on send) without the Thread primitives needing
// to know about sessions.
export const SessionStatusContext = createContext<SessionState | undefined>(undefined);

export const useSessionStatus = (): SessionState | undefined => useContext(SessionStatusContext);
