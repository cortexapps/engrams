"use client";

import * as ResizablePrimitive from "react-resizable-panels";

import { cn } from "@/lib/utils";

// Thin wrapper over react-resizable-panels in the Aston-racing voice: the
// divider is a hairline (--border) that lifts to the lime accent on hover/drag
// — the same "the accent marks the live thing" grammar as the tab underline.
// A wider invisible hit-area (::after) makes the 1px line comfortable to grab
// without drawing a fat rule. `ResizablePanel` spreads `ref` straight through
// (React 19: ref is a prop), so callers can drive collapse()/expand() via an
// ImperativePanelHandle.

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
        "relative flex w-px shrink-0 items-stretch bg-border transition-colors",
        // the grab zone reaches past the hairline on both sides
        "after:absolute after:inset-y-0 after:left-1/2 after:w-3 after:-translate-x-1/2",
        "focus-visible:outline-none data-[resize-handle-state=hover]:bg-primary data-[resize-handle-state=drag]:bg-primary",
        "focus-visible:bg-primary",
        className,
      )}
      {...props}
    />
  );
}

export { ResizablePanelGroup, ResizablePanel, ResizableHandle };
