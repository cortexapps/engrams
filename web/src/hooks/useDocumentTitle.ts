import { useEffect } from "react";

/**
 * ADR 0107: set the browser-tab title while mounted, restoring the previous
 * title on unmount. The `●` prefix is the whole attention affordance — it
 * survives pinned tabs, where only the favicon+glyph area stays visible.
 */
export function useDocumentTitle(title: string | null) {
  useEffect(() => {
    if (title == null) return;
    const previous = document.title;
    document.title = title;
    return () => {
      document.title = previous;
    };
  }, [title]);
}
