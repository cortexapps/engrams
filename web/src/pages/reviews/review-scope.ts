import { useCallback, useSyncExternalStore } from "react";

import { useConnectorAccount } from "../../hooks/useCredentials";

// Which pull requests the reviews ledger and rail show: every reviewed PR, or
// only the ones the reader opened. "Mine" means the PR author is the GitHub
// login of the reader's connected GitHub credential — reviews come from
// GitHub, and the login is the name GitHub records as the author.
//
// The choice is a per-browser preference, like the rail width, so the ledger
// and the rail read one key and stay in step: the custom event syncs the
// same tab, the `storage` event the other tabs.

export type ReviewScope = "all" | "mine";

const STORAGE_KEY = "engrams.reviewsScope";
const CHANGE_EVENT = "engrams-reviews-scope-change";

function read(): ReviewScope {
  if (typeof localStorage === "undefined") return "all";
  return localStorage.getItem(STORAGE_KEY) === "mine" ? "mine" : "all";
}

function subscribe(onChange: () => void): () => void {
  window.addEventListener("storage", onChange);
  window.addEventListener(CHANGE_EVENT, onChange);
  return () => {
    window.removeEventListener("storage", onChange);
    window.removeEventListener(CHANGE_EVENT, onChange);
  };
}

export interface ReviewScopeState {
  scope: ReviewScope;
  setScope: (scope: ReviewScope) => void;
  /** The reader's GitHub login, when their GitHub credential names one. */
  login: string | undefined;
  /** "mine" is chosen and the reader's GitHub account is still loading. */
  pending: boolean;
  /** "mine" is chosen but cannot apply: no GitHub credential, or one that
   * names no account (saved before engrams asked GitHub for the login). */
  unavailable: "not_connected" | "no_login" | undefined;
  /** The author filter the scope adds to the list query. */
  authors: string[] | undefined;
}

export function useReviewScope(): ReviewScopeState {
  const scope = useSyncExternalStore(subscribe, read, () => "all" as const);
  const github = useConnectorAccount("github");

  const setScope = useCallback((next: ReviewScope) => {
    localStorage.setItem(STORAGE_KEY, next);
    window.dispatchEvent(new Event(CHANGE_EVENT));
  }, []);

  const login = github.name;
  const mine = scope === "mine";
  // "Mine" without a login says why, rather than showing every PR under a
  // "Mine" label or an empty list that reads as "you have no pull requests".
  const unavailable =
    !mine || github.isPending || login
      ? undefined
      : github.connected
        ? "no_login"
        : "not_connected";

  return {
    scope,
    setScope,
    login,
    pending: mine && github.isPending,
    unavailable,
    authors: mine && login ? [login] : undefined,
  };
}
