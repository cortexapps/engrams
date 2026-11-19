/** The color control for a tag: quick picks, a random color, a hex field and
 * the browser's color picker. It passes on what the admin enters unchecked;
 * the server owns the color rule. */

import { Shuffle } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { randomTagColor, TAG_QUICK_PICKS } from "./colors";

/** The native picker accepts only `#rrggbb`; while the hex field holds a
 * partial value, it shows black. */
function pickerValue(value: string): string {
  return /^#[0-9a-f]{6}$/i.test(value) ? value.toLowerCase() : "#000000";
}

export function TagColorField({
  value,
  onChange,
}: {
  value: string;
  onChange: (hex: string) => void;
}) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <div className="flex items-center gap-1">
        {TAG_QUICK_PICKS.map((hex) => (
          <button
            key={hex}
            type="button"
            aria-label={`Use color ${hex}`}
            aria-pressed={value.trim().toLowerCase() === hex}
            onClick={() => onChange(hex)}
            className="size-5 rounded-sm border outline-none focus-visible:ring-2 focus-visible:ring-ring/50 aria-pressed:ring-2 aria-pressed:ring-ring aria-pressed:ring-offset-1"
            style={{ backgroundColor: hex }}
          />
        ))}
      </div>
      <Button
        type="button"
        variant="ghost"
        size="icon-sm"
        aria-label="Random color"
        onClick={() => onChange(randomTagColor())}
      >
        <Shuffle />
      </Button>
      <Input
        aria-label="Hex color"
        value={value}
        onChange={(event) => onChange(event.target.value)}
        spellCheck={false}
        className="h-8 w-28 font-mono"
      />
      <input
        type="color"
        aria-label="Pick a color"
        value={pickerValue(value)}
        onChange={(event) => onChange(event.target.value)}
        className="size-8 cursor-pointer rounded-sm border bg-transparent p-0.5"
      />
    </div>
  );
}
