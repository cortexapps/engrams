import { cn } from "@/lib/utils";
import { textVariants } from "@/components/ui/text";

// Typeset tab labels with a hairline underline below the active one.
// Not pill buttons. Not icons. Just the instrument-label voice (Saira tracked
// caps) with a 2px lime rule under whichever is selected (the racing accent's
// one appearance here). Switching is instant — tabs are state, not motion.

export interface Tab<T extends string> {
  id: T;
  label: string;
}

export interface TabRowProps<T extends string> {
  tabs: Tab<T>[];
  active: T;
  onChange: (id: T) => void;
  /** Optional element rendered to the right of the tabs (e.g. counts). */
  right?: React.ReactNode;
}

export function TabRow<T extends string>({ tabs, active, onChange, right }: TabRowProps<T>) {
  return (
    <div className="mb-4 flex items-baseline justify-between border-b pb-2">
      <nav className="-mb-2.5 flex items-baseline gap-5">
        {tabs.map((t) => {
          const isActive = t.id === active;
          return (
            <button
              key={t.id}
              type="button"
              onClick={() => onChange(t.id)}
              className={cn(
                textVariants({ variant: "label" }),
                "border-b-2 pb-2 transition-colors",
                isActive
                  ? "border-primary text-foreground"
                  : "border-transparent text-muted-foreground hover:text-foreground",
              )}
            >
              {t.label}
            </button>
          );
        })}
      </nav>
      {right && <div className="font-mono text-[0.7rem] text-muted-foreground">{right}</div>}
    </div>
  );
}
