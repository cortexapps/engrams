import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

afterEach(() => cleanup());

if (!window.matchMedia) {
  window.matchMedia = (query: string) =>
    ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }) as unknown as MediaQueryList;
}

if (!("ResizeObserver" in window)) {
  // @ts-expect-error minimal stub
  window.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}

// jsdom is layout-free, so element scrolling methods don't exist and throw when
// libraries call them — assistant-ui's <Thread> auto-scrolls to the bottom on
// mount via requestAnimationFrame (`viewport.scrollTo(...)`), which surfaced as
// repeated "div.scrollTo is not a function" uncaught exceptions. No-op them, the
// same way matchMedia/ResizeObserver are stubbed above.
// Same story for pointer capture, which a drag handle claims on pointerdown
// (SidebarResizeHandle): jsdom ships no implementation at all.
if (typeof Element.prototype.setPointerCapture !== "function") {
  Element.prototype.setPointerCapture = () => {};
  Element.prototype.releasePointerCapture = () => {};
  Element.prototype.hasPointerCapture = () => false;
}

if (typeof Element.prototype.scrollTo !== "function") Element.prototype.scrollTo = () => {};
if (typeof Element.prototype.scrollBy !== "function") Element.prototype.scrollBy = () => {};
if (typeof Element.prototype.scrollIntoView !== "function")
  Element.prototype.scrollIntoView = () => {};
