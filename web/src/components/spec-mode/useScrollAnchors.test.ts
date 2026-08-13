import { act, renderHook } from "@testing-library/react";
import { createRef } from "react";
import { afterEach, describe, expect, test, vi } from "vitest";

import { useScrollAnchors } from "./useScrollAnchors";

afterEach(() => vi.unstubAllGlobals());

describe("useScrollAnchors", () => {
  test("waits for two animation frames and scrolls to offsetTop minus 72", () => {
    const callbacks: FrameRequestCallback[] = [];
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn((callback: FrameRequestCallback) => {
        callbacks.push(callback);
        return callbacks.length;
      }),
    );
    vi.stubGlobal("cancelAnimationFrame", vi.fn());

    const pane = document.createElement("div");
    const section = document.createElement("section");
    section.dataset.sectionId = "api";
    Object.defineProperty(section, "offsetTop", { value: 260 });
    pane.append(section);
    const scrollTo = vi.fn();
    pane.scrollTo = scrollTo;
    const ref = createRef<HTMLElement>();
    ref.current = pane;
    const { result } = renderHook(() => useScrollAnchors(ref));

    act(() => result.current.scrollToSection("api"));
    expect(scrollTo).not.toHaveBeenCalled();
    act(() => callbacks.shift()!(0));
    expect(scrollTo).not.toHaveBeenCalled();
    act(() => callbacks.shift()!(1));

    expect(scrollTo).toHaveBeenCalledWith({ top: 188 });
  });

  test("does nothing when the section is absent", () => {
    const callbacks: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
      callbacks.push(callback);
      return callbacks.length;
    });
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
    const pane = document.createElement("div");
    pane.scrollTo = vi.fn();
    const ref = createRef<HTMLElement>();
    ref.current = pane;
    const { result } = renderHook(() => useScrollAnchors(ref));

    act(() => result.current.scrollToSection("missing"));
    act(() => callbacks.shift()!(0));
    act(() => callbacks.shift()!(1));

    expect(pane.scrollTo).not.toHaveBeenCalled();
  });
});
