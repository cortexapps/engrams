import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";
import { notifyManager } from "@tanstack/react-query";

afterEach(() => cleanup());

// TanStack Query delivers observer notifications in a batch on a system
// `setTimeout(0)`. A batch queued in the last moments of a test file can run
// after vitest has torn down that file's jsdom environment, and React then
// reads `window` and throws "window is not defined". Vitest reports it as an
// unhandled error and fails the run although every test passed. A microtask
// runs before the test's teardown, so no notification can outlive its test.
notifyManager.setScheduler(queueMicrotask);

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
