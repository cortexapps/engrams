"use client";

import * as ResizablePrimitive from "react-resizable-panels";

import { cn } from "@/lib/utils";

// Thin wrapper over react-resizable-panels in the Aston-racing voice: the
// handle IS the gutter between two surfaces — nothing at rest, so the cover
// shows through, and a lime pill under the pointer. The same grammar as
// `SidebarResizeHandle`, which resizes a rail by hand rather than through this
// library; a drag handle should not look like two different controls depending
// on which edge of the page it sits on.
// `ResizablePanel` forwards `panelRef` straight through, so callers can drive
// collapse()/expand()/resize() via a PanelImperativeHandle.
//
// v4 renamed the primitives (`PanelGroup` -> `Group`, `PanelResizeHandle` ->
// `Separator`) and, more consequentially, changed what a bare number MEANS:
//
//   > Numbers are interpreted as pixels (e.g. `defaultSize={200}` is 200 pixels)
//   > Strings without explicit units are interpreted as percentage
//
// Every size in this app is a percentage, and under v4 a percentage written as
// a number still type-checks — it just silently becomes pixels. So the wrapper
// owns the convention instead of leaving it at each call site: a plain number
// means percent (v2's meaning), and a string passes through untouched, which
// keeps explicit units like "200px" or "3rem" available if anything wants them.

/**
 * A percentage size in the form react-resizable-panels v4 reads as percent.
 *
 * Use for the imperative API (`PanelImperativeHandle.resize`), which takes the
 * same `number | string` union as the props and applies the same pixel-vs-%
 * rule. The size *props* go through {@link ResizablePanel}, which applies this
 * for you.
 */
export function percentSize(value: number): string {
  return String(value);
}

/** Percent-by-default for a size prop; strings keep their explicit units. */
function asPercent(value: number | string | undefined): number | string | undefined {
  return typeof value === "number" ? percentSize(value) : value;
}

function ResizablePanelGroup({
  className,
  orientation = "horizontal",
  ...props
}: React.ComponentProps<typeof ResizablePrimitive.Group>) {
  return (
    <ResizablePrimitive.Group
      data-slot="resizable-panel-group"
      // v4 emits only `data-group` on the group element — the direction hook
      // this laid its column flip on (`data-panel-group-direction`) is gone.
      // We already receive the orientation, so publish it ourselves rather
      // than reach for a library internal.
      data-orientation={orientation}
      orientation={orientation}
      className={cn("flex h-full w-full data-[orientation=vertical]:flex-col", className)}
      {...props}
    />
  );
}

function ResizablePanel({
  collapsedSize,
  defaultSize,
  maxSize,
  minSize,
  ...props
}: React.ComponentProps<typeof ResizablePrimitive.Panel>) {
  return (
    <ResizablePrimitive.Panel
      data-slot="resizable-panel"
      collapsedSize={asPercent(collapsedSize)}
      defaultSize={asPercent(defaultSize)}
      maxSize={asPercent(maxSize)}
      minSize={asPercent(minSize)}
      {...props}
    />
  );
}

function ResizableHandle({
  className,
  ...props
}: React.ComponentProps<typeof ResizablePrimitive.Separator>) {
  return (
    <ResizablePrimitive.Separator
      data-slot="resizable-handle"
      className={cn(
        "relative flex w-2 shrink-0 items-stretch bg-transparent",
        "after:absolute after:inset-y-0 after:left-1/2 after:w-2 after:-translate-x-1/2 after:rounded-full after:transition-colors",
        // v4 dropped `data-resize-handle-state`; upstream's guidance for the
        // separator is to style hover and active directly, so the drag accent
        // rides `:active` instead of a data attribute.
        "focus-visible:outline-none hover:after:bg-primary/60 focus-visible:after:bg-primary",
        "active:after:bg-primary",
        className,
      )}
      {...props}
    />
  );
}

export { ResizablePanelGroup, ResizablePanel, ResizableHandle };
