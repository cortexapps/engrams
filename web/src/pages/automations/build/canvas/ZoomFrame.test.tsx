import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { ZoomFrame } from "./ZoomFrame";

// jsdom is layout-free: give the viewport a client width and the diagram an
// offset width, so the fit has something to measure.
let viewportWidth = 0;
const CONTENT_WIDTH = 1000;
const saved: Record<string, PropertyDescriptor | undefined> = {};

beforeEach(() => {
  saved.clientWidth = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientWidth");
  saved.offsetWidth = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "offsetWidth");
  Object.defineProperty(HTMLElement.prototype, "clientWidth", {
    configurable: true,
    get: () => viewportWidth,
  });
  Object.defineProperty(HTMLElement.prototype, "offsetWidth", {
    configurable: true,
    get: () => CONTENT_WIDTH,
  });
});

afterEach(() => {
  for (const key of ["clientWidth", "offsetWidth"]) {
    const descriptor = saved[key];
    if (descriptor) Object.defineProperty(HTMLElement.prototype, key, descriptor);
    else delete (HTMLElement.prototype as unknown as Record<string, unknown>)[key];
  }
});

function mount() {
  render(
    <ZoomFrame>
      <div data-testid="diagram" />
    </ZoomFrame>,
  );
  return () => screen.getByTestId("zoom-level").textContent;
}

describe("ZoomFrame", () => {
  it("fits the diagram to the width, between the minimum and full size", () => {
    viewportWidth = 748; // 700 inside the 24px padding on each side
    expect(mount()()).toBe("70%");
  });

  it("never grows the diagram past full size", () => {
    viewportWidth = 3000;
    const level = mount();
    expect(level()).toBe("100%");
    expect(screen.getByRole("button", { name: "Zoom in" })).toHaveProperty("disabled", true);
  });

  it("stops at the minimum scale and leaves the rest to the scroll area", () => {
    viewportWidth = 300;
    const level = mount();
    expect(level()).toBe("50%");
    expect(screen.getByRole("button", { name: "Zoom out" })).toHaveProperty("disabled", true);
  });

  it("steps from the fitted scale, and Fit returns to it", () => {
    viewportWidth = 748;
    const level = mount();
    fireEvent.click(screen.getByRole("button", { name: "Zoom in" }));
    expect(level()).toBe("80%");
    expect(screen.getByRole("button", { name: "Fit" }).getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(screen.getByRole("button", { name: "Fit" }));
    expect(level()).toBe("70%");
  });
});
