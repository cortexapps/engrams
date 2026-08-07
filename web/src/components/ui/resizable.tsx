"use client";

import * as ResizablePrimitive from "react-resizable-panels";

import { cn } from "@/lib/utils";

// Thin wrapper over react-resizable-panels in the Aston-racing voice: the
// handle IS the gutter between two surfaces — nothing at rest, so the cover
// shows through, and a lime pill under the pointer. The same grammar as
// `SidebarResizeHandle`, which resizes a rail by hand rather than through this
// library; a drag handle should not look like two different controls depending
// on which edge of the page it sits on.
// `ResizablePanel` spreads `ref` straight through (React 19: ref is a prop), so
// callers can drive collapse()/expand() via an ImperativePanelHandle.

function ResizablePanelGroup({
  className,
  ...props
}: React.ComponentProps<typeof ResizablePrimitive.PanelGroup>) {
  return (
    <ResizablePrimitive.PanelGroup
      data-slot="resizable-panel-group"
      className={cn("flex h-full w-full data-[panel-group-direction=vertical]:flex-col", className)}
      {...props}
    />
  );
}

function ResizablePanel(props: React.ComponentProps<typeof ResizablePrimitive.Panel>) {
  return <ResizablePrimitive.Panel data-slot="resizable-panel" {...props} />;
}

function ResizableHandle({
  className,
  ...props
}: React.ComponentProps<typeof ResizablePrimitive.PanelResizeHandle>) {
  return (
    <ResizablePrimitive.PanelResizeHandle
      data-slot="resizable-handle"
      className={cn(
        "relative flex w-2 shrink-0 items-stretch bg-transparent",
        "after:absolute after:inset-y-0 after:left-1/2 after:w-2 after:-translate-x-1/2 after:rounded-full after:transition-colors",
        "focus-visible:outline-none hover:after:bg-primary/60 focus-visible:after:bg-primary",
        "data-[resize-handle-state=drag]:after:bg-primary",
        className,
      )}
      {...props}
    />
  );
}

export { ResizablePanelGroup, ResizablePanel, ResizableHandle };
