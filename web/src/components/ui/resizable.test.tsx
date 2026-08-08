import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { render, screen } from "@testing-library/react";

import { ResizablePanelGroup, ResizablePanel, ResizableHandle, percentSize } from "./resizable";

// jsdom is layout-free: getBoundingClientRect and offsetWidth both read 0, and
// react-resizable-panels needs a measured group before it can resolve a size
// into a flex basis (with no measurement it falls back to an even split and the
// unit question never gets asked). Give it a 1000px-wide group so percentages
// and pixels resolve to visibly different answers.
const GROUP_WIDTH = 1000;
let originalRect: typeof Element.prototype.getBoundingClientRect;
let originalOffsetWidth: PropertyDescriptor | undefined;

beforeEach(() => {
  originalRect = Element.prototype.getBoundingClientRect;
  originalOffsetWidth = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "offsetWidth");
  Element.prototype.getBoundingClientRect = function () {
    return {
      width: GROUP_WIDTH,
      height: 200,
      top: 0,
      left: 0,
      right: GROUP_WIDTH,
      bottom: 200,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    } as DOMRect;
  };
  Object.defineProperty(HTMLElement.prototype, "offsetWidth", {
    configurable: true,
    get: () => GROUP_WIDTH,
  });
});

afterEach(() => {
  Element.prototype.getBoundingClientRect = originalRect;
  if (originalOffsetWidth) {
    Object.defineProperty(HTMLElement.prototype, "offsetWidth", originalOffsetWidth);
  } else {
    delete (HTMLElement.prototype as unknown as Record<string, unknown>).offsetWidth;
  }
});

// react-resizable-panels v4 reinterpreted bare numbers as PIXELS; a percentage
// written as a number still type-checks and silently means px. Every size in
// this app is a percentage, and one of them (`engram.workpane`) is persisted in
// localStorage, so a returning developer's stored 42 would come back as 42px.
//
// These tests exercise the REAL library rather than the mock that
// SessionDetail.test.tsx installs, because the mock cannot see the unit at all
// — it is exactly the layer where this regression would hide.

describe("percentSize", () => {
  test("renders a number in the form v4 reads as percent, not pixels", () => {
    // Unit-less string = percentage. A bare number would be pixels.
    expect(percentSize(42)).toBe("42");
    expect(percentSize(0)).toBe("0");
    expect(typeof percentSize(42)).toBe("string");
  });
});

describe("ResizablePanel size units", () => {
  test("lays a numeric defaultSize out as a percentage of the group", () => {
    render(
      <div style={{ width: "1000px", height: "200px" }}>
        <ResizablePanelGroup orientation="horizontal">
          <ResizablePanel id="left" defaultSize={30} minSize={10} collapsedSize={0} collapsible>
            left
          </ResizablePanel>
          <ResizableHandle />
          <ResizablePanel id="right" defaultSize={70} minSize={10}>
            right
          </ResizablePanel>
        </ResizablePanelGroup>
      </div>,
    );

    // The panel's own element carries the flex basis the library computed.
    // 30 must mean 30% — under a pixel reading it would be 30px of 1000.
    const left = document.querySelector('[data-panel][id="left"]') as HTMLElement;
    const right = document.querySelector('[data-panel][id="right"]') as HTMLElement;
    expect(left).not.toBeNull();
    expect(right).not.toBeNull();
    expect(left.style.flexGrow).toBe("30");
    expect(right.style.flexGrow).toBe("70");
  });

  // The unit contract in one assertion, without hard-coding whatever width the
  // stub makes the library measure: a bare number and the same number as a
  // unit-less string must agree (both percent), and the px spelling must NOT
  // agree with either (it is a different unit, so a different flex basis).
  test("treats a number and a unit-less string alike, and px differently", () => {
    const basisOf = (defaultSize: number | string) => {
      const { unmount } = render(
        <div style={{ width: "1000px", height: "200px" }}>
          <ResizablePanelGroup orientation="horizontal">
            <ResizablePanel id="probe" defaultSize={defaultSize}>
              probe
            </ResizablePanel>
            <ResizableHandle />
            <ResizablePanel id="rest">rest</ResizablePanel>
          </ResizablePanelGroup>
        </div>,
      );
      const el = document.querySelector('[data-panel][id="probe"]') as HTMLElement;
      const basis = el.style.flexGrow;
      unmount();
      return basis;
    };

    expect(basisOf(30)).toBe("30");
    expect(basisOf("30")).toBe("30");
    // If the wrapper stringified "30px" into a percentage, this would be 30.
    expect(basisOf("30px")).not.toBe("30");
    expect(Number(basisOf("30px"))).toBeLessThan(30);
  });
});

describe("ResizablePanelGroup orientation", () => {
  // v4 dropped `data-panel-group-direction`, which the column flip was styled
  // off. The wrapper republishes it as `data-orientation`; the Tailwind
  // selector `data-[orientation=vertical]:flex-col` has to keep matching.
  test("publishes the orientation the vertical column flip is styled off", () => {
    const { rerender } = render(
      <ResizablePanelGroup orientation="vertical">
        <ResizablePanel id="a">a</ResizablePanel>
      </ResizablePanelGroup>,
    );
    const group = () => document.querySelector("[data-slot=resizable-panel-group]") as HTMLElement;
    expect(group().getAttribute("data-orientation")).toBe("vertical");
    expect(group().className).toContain("data-[orientation=vertical]:flex-col");

    rerender(
      <ResizablePanelGroup orientation="horizontal">
        <ResizablePanel id="a">a</ResizablePanel>
      </ResizablePanelGroup>,
    );
    expect(group().getAttribute("data-orientation")).toBe("horizontal");
  });

  test("defaults to horizontal", () => {
    render(
      <ResizablePanelGroup>
        <ResizablePanel id="a">a</ResizablePanel>
      </ResizablePanelGroup>,
    );
    const group = document.querySelector("[data-slot=resizable-panel-group]") as HTMLElement;
    expect(group.getAttribute("data-orientation")).toBe("horizontal");
  });
});

describe("ResizableHandle", () => {
  // v4 emits `data-separator` and no longer emits `data-resize-handle-state`,
  // so the drag accent rides `:active`. Pin both halves: the element the
  // styles hang off exists, and the class list no longer references the dead
  // attribute (a stale selector matches nothing and fails silently).
  test("renders v4's separator and styles drag off :active, not a dead attribute", () => {
    render(
      <ResizablePanelGroup orientation="horizontal">
        <ResizablePanel id="a">a</ResizablePanel>
        <ResizableHandle />
        <ResizablePanel id="b">b</ResizablePanel>
      </ResizablePanelGroup>,
    );

    const handle = document.querySelector("[data-slot=resizable-handle]") as HTMLElement;
    expect(handle).not.toBeNull();
    expect(handle.hasAttribute("data-separator")).toBe(true);
    expect(handle.getAttribute("role")).toBe("separator");
    expect(handle.className).toContain("active:after:bg-primary");
    expect(handle.className).not.toContain("resize-handle-state");
    expect(handle.className).not.toContain("panel-group-direction");
  });

  test("is reachable by its separator role", () => {
    render(
      <ResizablePanelGroup orientation="horizontal">
        <ResizablePanel id="a">a</ResizablePanel>
        <ResizableHandle />
        <ResizablePanel id="b">b</ResizablePanel>
      </ResizablePanelGroup>,
    );
    expect(screen.getAllByRole("separator").length).toBe(1);
  });
});
