import { expect, test, beforeEach } from "vitest";
import { render, act } from "@testing-library/react";
import * as React from "react";
import { SidebarProvider, useSidebar } from "@/components/ui/sidebar";

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
