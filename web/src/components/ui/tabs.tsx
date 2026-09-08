"use client";

import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { motion } from "framer-motion";
import { Tabs as TabsPrimitive } from "radix-ui";

import { cn } from "@/lib/utils";

// Stock shadcn Tabs, styled as pill tabs (30px, 8px corners, the active pill
// on `--secondary` at 600, no track, no shadow) — with one addition: the
// active pill SLIDES between triggers (180ms) instead of blinking. Radix keeps
// the state and the a11y; a small context mirrors the active value so each
// trigger knows whether it hosts the pill, and framer-motion's `layoutId`
// moves one pill per list.

interface TabsState {
  value: string | undefined;
  /** One pill per list: the layoutId's scope. */
  listId: string;
}
const TabsContext = React.createContext<TabsState>({ value: undefined, listId: "" });

function Tabs({
  className,
  orientation = "horizontal",
  value,
  defaultValue,
  onValueChange,
  ...props
}: React.ComponentProps<typeof TabsPrimitive.Root>) {
  // Mirror the active value whether the caller controls it or not.
  const [inner, setInner] = React.useState(defaultValue);
  const active = value ?? inner;
  const listId = React.useId();
  return (
    <TabsContext.Provider value={{ value: active, listId }}>
      <TabsPrimitive.Root
        data-slot="tabs"
        data-orientation={orientation}
        orientation={orientation}
        value={value}
        defaultValue={defaultValue}
        onValueChange={(next) => {
          setInner(next);
          onValueChange?.(next);
        }}
        className={cn("group/tabs flex gap-2 data-[orientation=horizontal]:flex-col", className)}
        {...props}
      />
    </TabsContext.Provider>
  );
}

const tabsListVariants = cva(
  "group/tabs-list inline-flex w-fit items-center justify-center gap-1 text-muted-foreground group-data-[orientation=horizontal]/tabs:h-[30px] group-data-[orientation=vertical]/tabs:h-fit group-data-[orientation=vertical]/tabs:flex-col data-[variant=line]:rounded-none",
  {
    variants: {
      variant: {
        default: "bg-transparent",
        line: "bg-transparent",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

function TabsList({
  className,
  variant = "default",
  ...props
}: React.ComponentProps<typeof TabsPrimitive.List> & VariantProps<typeof tabsListVariants>) {
  return (
    <TabsPrimitive.List
      data-slot="tabs-list"
      data-variant={variant}
      className={cn(tabsListVariants({ variant }), className)}
      {...props}
    />
  );
}

function TabsTrigger({
  className,
  children,
  value,
  ...props
}: React.ComponentProps<typeof TabsPrimitive.Trigger>) {
  const { value: active, listId } = React.useContext(TabsContext);
  const isActive = active === value;
  return (
    <TabsPrimitive.Trigger
      data-slot="tabs-trigger"
      value={value}
      className={cn(
        "relative inline-flex h-full flex-1 items-center justify-center gap-1.5 rounded-sm px-2.5 text-sm font-medium whitespace-nowrap text-muted-foreground transition-colors duration-150 group-data-[orientation=vertical]/tabs:w-full group-data-[orientation=vertical]/tabs:justify-start hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/60 focus-visible:outline-none disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
        "group-data-[variant=line]/tabs-list:hover:bg-transparent",
        "data-[state=active]:font-semibold data-[state=active]:text-foreground",
        className,
      )}
      {...props}
    >
      {isActive && (
        <motion.span
          layoutId={`${listId}-pill`}
          aria-hidden
          className="absolute inset-0 -z-10 rounded-sm bg-secondary group-data-[variant=line]/tabs-list:bg-transparent"
          transition={{ duration: 0.18, ease: [0.2, 0.7, 0.2, 1] }}
        />
      )}
      {children}
    </TabsPrimitive.Trigger>
  );
}

function TabsContent({ className, ...props }: React.ComponentProps<typeof TabsPrimitive.Content>) {
  return (
    <TabsPrimitive.Content
      data-slot="tabs-content"
      className={cn("flex-1 outline-none", className)}
      {...props}
    />
  );
}

export { Tabs, TabsList, TabsTrigger, TabsContent, tabsListVariants };
