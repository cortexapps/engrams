/** The seam between the canvas and the inspector.
 *
 * The inspector used to be a fixed 360px `<aside>`, which is too narrow for the
 * code block's editor. It is a resizable panel now, so these tests pin the two
 * things that would silently take the drag away again: the separator itself,
 * and the unit the stored width comes back in (react-resizable-panels v4 reads
 * a bare number as PIXELS, so a stored 42 returning as 42px is the failure mode
 * this guards).
 *
 * The real `@/components/ui/resizable` runs here on purpose — a mock sees
 * neither property.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { screen } from "@testing-library/react";

import { renderWithProviders } from "@/test-utils";
import type { AutomationDefinition } from "@/lib/automation-blocks";

import { BuildTab } from "./BuildTab";

const INSPECTOR_WIDTH_KEY = "engram.automation-inspector";

// No blocks: the trigger inspector is the lightest body to hang the layout
// assertions off, and nothing here is about what the inspector holds.
const definition: AutomationDefinition = {
  engine: 1,
  trigger: { kind: "manual" },
  blocks: [],
  inputsSchema: [],
  settings: { endSessionsOnFinish: false },
};

// jsdom is layout-free: every rect reads 0, and react-resizable-panels needs a
// measured group before it can resolve a size into a flex basis (with no
// measurement it falls back to an even split and the unit question is never
// asked). Give it a 1000px-wide group, the same stub ui/resizable.test.tsx uses.
const GROUP_WIDTH = 1000;
let originalRect: typeof Element.prototype.getBoundingClientRect;
let originalOffsetWidth: PropertyDescriptor | undefined;

beforeEach(() => {
  localStorage.clear();
  originalRect = Element.prototype.getBoundingClientRect;
  originalOffsetWidth = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "offsetWidth");
  Element.prototype.getBoundingClientRect = function () {
    return {
      width: GROUP_WIDTH,
      height: 600,
      top: 0,
      left: 0,
      right: GROUP_WIDTH,
      bottom: 600,
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
  localStorage.clear();
  Element.prototype.getBoundingClientRect = originalRect;
  if (originalOffsetWidth) {
    Object.defineProperty(HTMLElement.prototype, "offsetWidth", originalOffsetWidth);
  } else {
    delete (HTMLElement.prototype as unknown as Record<string, unknown>).offsetWidth;
  }
});

function mount() {
  return renderWithProviders(
    <BuildTab
      definition={definition}
      entrypointId="main"
      onSelectEntrypoint={vi.fn()}
      onChange={vi.fn()}
      builtin={false}
      errors={[]}
      triggerSummaryFor={() => "Manual"}
    />,
  );
}

/** The share of the group the inspector opened at. The library writes the
 * resolved layout as the panel's own flex-grow. */
function inspectorShare(): number {
  const panel = document.querySelector('[data-panel][id="build-inspector"]') as HTMLElement;
  return Number(panel.style.flexGrow);
}

describe("BuildTab inspector seam", () => {
  it("puts a drag separator between the canvas and the inspector", async () => {
    mount();
    await screen.findByTestId("inspector");

    const separators = screen.getAllByRole("separator");
    expect(separators.length).toBe(1);
    const handle = separators[0];
    expect(handle.getAttribute("data-slot")).toBe("resizable-handle");
    // The seam has to sit BETWEEN the two panels, or the drag moves the wrong
    // edge.
    expect(handle.previousElementSibling?.id).toBe("build-canvas");
    expect(handle.nextElementSibling?.id).toBe("build-inspector");
  });

  it("restores a stored width as a percentage, not as pixels", async () => {
    localStorage.setItem(INSPECTOR_WIDTH_KEY, "55");
    mount();
    await screen.findByTestId("inspector");

    // 55 must mean 55% of the group. Read as pixels it would be 55 of 1000,
    // i.e. the inspector would come back a tenth of the width it was left at.
    expect(inspectorShare()).toBeCloseTo(55, 0);
  });

  it("ignores a garbled or degenerate stored width", async () => {
    const shareFor = async (stored: string | null) => {
      if (stored === null) localStorage.removeItem(INSPECTOR_WIDTH_KEY);
      else localStorage.setItem(INSPECTOR_WIDTH_KEY, stored);
      const { unmount } = mount();
      await screen.findByTestId("inspector");
      const share = inspectorShare();
      unmount();
      return share;
    };

    const garbled = await shareFor("wide please");
    const tooWide = await shareFor("140");
    const unset = await shareFor(null);

    // Both bad readings fall back to the designed 360px default.
    expect(garbled).toBeCloseTo(unset, 5);
    expect(tooWide).toBeCloseTo(unset, 5);
    // And that default is a real split, not a collapsed or full-width panel.
    expect(unset).toBeGreaterThan(0);
    expect(unset).toBeLessThan(100);
  });
});
