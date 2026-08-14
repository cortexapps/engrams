import { useCallback, useEffect, useRef, type RefObject } from "react";

export interface ScrollAnchors {
  scrollToSection: (sectionId: string) => void;
}

/** Scroll a document section below the sticky 72 px chrome after layout settles. */
export function useScrollAnchors(documentPane: RefObject<HTMLElement | null>): ScrollAnchors {
  const frames = useRef<number[]>([]);

  useEffect(
    () => () => {
      for (const frame of frames.current) cancelAnimationFrame(frame);
      frames.current = [];
    },
    [],
  );

  const scrollToSection = useCallback(
    (sectionId: string) => {
      const first = requestAnimationFrame(() => {
        const second = requestAnimationFrame(() => {
          const pane = documentPane.current;
          if (!pane) return;
          const section = Array.from(pane.querySelectorAll<HTMLElement>("[data-section-id]")).find(
            (candidate) => candidate.dataset.sectionId === sectionId,
          );
          if (!section) return;
          pane.scrollTo({ top: Math.max(0, section.offsetTop - 72) });
        });
        frames.current.push(second);
      });
      frames.current.push(first);
    },
    [documentPane],
  );

  return { scrollToSection };
}
