import { expect, test, beforeEach } from "vitest";
import { render, act, fireEvent } from "@testing-library/react";
import * as React from "react";
import {
  Sidebar,
  SidebarProvider,
  SidebarResizeHandle,
  useSidebar,
  useSidebarWidth,
} from "@/components/ui/sidebar";

const STORAGE_KEY = "sidebar_state";

function SidebarState() {
  const { state, toggleSidebar } = useSidebar();
  return (
    <button data-testid="toggle" data-state={state} onClick={toggleSidebar}>
      {state}
    </button>
  );
}

function renderSidebar(props?: React.ComponentProps<typeof SidebarProvider>) {
  return render(
    <SidebarProvider {...props}>
      <SidebarState />
    </SidebarProvider>,
  );
}

beforeEach(() => {
  localStorage.clear();
});

test("defaults to expanded when no stored state", () => {
  const { getByTestId } = renderSidebar();
  expect(getByTestId("toggle").dataset.state).toBe("expanded");
});

test("respects defaultOpen=false when no stored state", () => {
  const { getByTestId } = renderSidebar({ defaultOpen: false });
  expect(getByTestId("toggle").dataset.state).toBe("collapsed");
});

test("restores collapsed state from localStorage on mount", () => {
  localStorage.setItem(STORAGE_KEY, "false");
  const { getByTestId } = renderSidebar();
  expect(getByTestId("toggle").dataset.state).toBe("collapsed");
});

test("restores expanded state from localStorage on mount", () => {
  localStorage.setItem(STORAGE_KEY, "true");
  const { getByTestId } = renderSidebar({ defaultOpen: false });
  expect(getByTestId("toggle").dataset.state).toBe("expanded");
});

test("persists state to localStorage when toggled", () => {
  const { getByTestId } = renderSidebar();
  expect(localStorage.getItem(STORAGE_KEY)).toBeNull();

  act(() => getByTestId("toggle").click());
  expect(localStorage.getItem(STORAGE_KEY)).toBe("false");

  act(() => getByTestId("toggle").click());
  expect(localStorage.getItem(STORAGE_KEY)).toBe("true");
});

// --- useSidebarWidth / SidebarResizeHandle -------------------------------

const WIDTH_KEY = "test_rail_width";

function ResizableRail() {
  const [style, handle] = useSidebarWidth(WIDTH_KEY);
  return (
    <SidebarProvider data-testid="wrapper" style={style}>
      <Sidebar collapsible="none">
        <SidebarResizeHandle {...handle} label="Resize rail" />
      </Sidebar>
    </SidebarProvider>
  );
}

function renderRail() {
  const { getByTestId, getByRole } = render(<ResizableRail />);
  return { wrapper: getByTestId("wrapper"), handle: getByRole("separator") };
}

const widthVar = (wrapper: HTMLElement) => wrapper.style.getPropertyValue("--sidebar-width");

test("leaves --sidebar-width at the rem default when nothing is stored", () => {
  const { wrapper } = renderRail();
  expect(widthVar(wrapper)).toBe("16rem");
});

test("restores a stored width in px", () => {
  localStorage.setItem(WIDTH_KEY, "340");
  const { wrapper } = renderRail();
  expect(widthVar(wrapper)).toBe("340px");
});

test("clamps a stored width above the ceiling", () => {
  localStorage.setItem(WIDTH_KEY, "9000");
  expect(widthVar(renderRail().wrapper)).toBe("560px");
});

test("clamps a stored width below the floor", () => {
  localStorage.setItem(WIDTH_KEY, "10");
  expect(widthVar(renderRail().wrapper)).toBe("200px");
});

test("ignores a garbled stored width", () => {
  localStorage.setItem(WIDTH_KEY, "wide please");
  expect(widthVar(renderRail().wrapper)).toBe("16rem");
});

test("a drag resizes the rail and persists the width on release", () => {
  localStorage.setItem(WIDTH_KEY, "300");
  const { wrapper, handle } = renderRail();

  act(() => {
    fireEvent.pointerDown(handle, { button: 0, pointerId: 1, clientX: 300 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 360 });
  });
  expect(widthVar(wrapper)).toBe("360px");
  // Nothing is written mid-drag — a pointermove per frame must not hit storage.
  expect(localStorage.getItem(WIDTH_KEY)).toBe("300");

  act(() => fireEvent.pointerUp(handle, { pointerId: 1 }));
  expect(localStorage.getItem(WIDTH_KEY)).toBe("360");
});

test("a drag past the bounds stops at the ceiling", () => {
  localStorage.setItem(WIDTH_KEY, "300");
  const { wrapper, handle } = renderRail();

  act(() => {
    fireEvent.pointerDown(handle, { button: 0, pointerId: 1, clientX: 300 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 2000 });
    fireEvent.pointerUp(handle, { pointerId: 1 });
  });
  expect(widthVar(wrapper)).toBe("560px");
  expect(localStorage.getItem(WIDTH_KEY)).toBe("560");
});

test("arrow keys nudge the width and persist it", () => {
  localStorage.setItem(WIDTH_KEY, "300");
  const { wrapper, handle } = renderRail();

  act(() => fireEvent.keyDown(handle, { key: "ArrowRight" }));
  expect(widthVar(wrapper)).toBe("316px");
  expect(localStorage.getItem(WIDTH_KEY)).toBe("316");

  act(() => fireEvent.keyDown(handle, { key: "ArrowLeft", shiftKey: true }));
  expect(widthVar(wrapper)).toBe("252px");
});

test("Home clears the stored width, back to the rem default", () => {
  localStorage.setItem(WIDTH_KEY, "300");
  const { wrapper, handle } = renderRail();

  act(() => fireEvent.keyDown(handle, { key: "Home" }));
  expect(widthVar(wrapper)).toBe("16rem");
  expect(localStorage.getItem(WIDTH_KEY)).toBeNull();
});

test("double-click resets the width", () => {
  localStorage.setItem(WIDTH_KEY, "300");
  const { wrapper, handle } = renderRail();

  act(() => fireEvent.doubleClick(handle));
  expect(widthVar(wrapper)).toBe("16rem");
  expect(localStorage.getItem(WIDTH_KEY)).toBeNull();
});
