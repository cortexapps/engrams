import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

// The single source of truth for type roles. Two orthogonal axes:
//
//   variant — the typographic ROLE (what font/size/weight/tracking).
//   tone    — the COLOR role (which ink token).
//
// `label` is the instrument-label voice: Saira (the display font) in tracked
// caps, NOT uppercased mono. That's what carries the racing identity at the
// label layer and frees mono to mean "machine data". Weight is 500, not 600 —
// a caption should recede beneath the figure it labels, not compete with it.
// Tracking is em-based so one value holds across the 0.62–0.72rem label range.
//
// `stat` is its partner: oversized tabular mono for the figure. The
// variant/tone split is the whole point of the gauge — a dark mono number
// (tone=default) under a quiet Saira caption (variant=label, tone=muted).
const textVariants = cva("", {
  variants: {
    variant: {
      // Page-title masthead voice (the PageHeading h1 lives here).
      display:
        "font-display text-3xl leading-tight font-semibold tracking-tight text-balance [font-stretch:108%]",
      // The masthead title when the page's subject IS machine data (a session
      // id, a digest): the mono lab-readout voice. NOT tracking-tight — negative
      // spacing on a long hex id runs the glyphs together (worse on the dark
      // ground); monospace wants its native advance, so tracking stays normal
      // and the size sits a notch below `display` so a 36-char id reads as a
      // legible title, not a cramped wall.
      displayMono: "font-mono text-xl leading-tight tracking-normal",
      heading: "font-display text-lg leading-snug font-semibold tracking-tight",
      body: "text-sm leading-relaxed",
      // Instrument label / eyebrow / table header / tab. Callers set color via
      // `tone` and may override size; the default 0.7rem fits most labels.
      label: "font-display text-[0.7rem] leading-none font-medium tracking-[0.1em] uppercase",
      // The figure in a gauge: big, aligned, machine.
      stat: "font-mono text-2xl leading-none tabular-nums",
      code: "font-mono text-[0.8rem]",
    },
    tone: {
      default: "text-foreground",
      muted: "text-muted-foreground",
      primary: "text-primary",
      destructive: "text-destructive",
      inherit: "",
    },
  },
  defaultVariants: { variant: "body", tone: "inherit" },
});

type TextVariantProps = VariantProps<typeof textVariants>;

type TextProps<T extends React.ElementType> = TextVariantProps & {
  /** The element/component to render. Defaults to `p`. */
  as?: T;
} & Omit<React.ComponentPropsWithoutRef<T>, keyof TextVariantProps | "as">;

// Polymorphic, typesafe: `as` widens the accepted props to the chosen element
// (React 19 takes `ref` as a normal prop, so no forwardRef ceremony needed).
function Text<T extends React.ElementType = "p">({
  as,
  variant,
  tone,
  className,
  ...props
}: TextProps<T>) {
  const Comp = (as ?? "p") as React.ElementType;
  return <Comp className={cn(textVariants({ variant, tone }), className)} {...props} />;
}

export { Text, textVariants };
