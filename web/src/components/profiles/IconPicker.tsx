import { useState } from "react";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Button } from "@/components/ui/button";
import { ProfileIcon, PROFILE_ICON_CHOICES } from "./ProfileIcon";

export function IconPicker({
  value,
  onChange,
  id,
}: {
  value: string;
  onChange: (name: string) => void;
  /** Binds a <label htmlFor> to the trigger (buttons are labelable). */
  id?: string;
}) {
  const [open, setOpen] = useState(false);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          id={id}
          type="button"
          variant="outline"
          className="w-full justify-start gap-2"
          data-testid="icon-picker"
        >
          <ProfileIcon name={value} className="size-4" />
          <span className="text-muted-foreground">{value}</span>
        </Button>
      </PopoverTrigger>
      <PopoverContent className="p-0" align="start">
        <Command>
          <CommandInput placeholder="Search icons…" />
          <CommandList>
            <CommandEmpty>No icons found.</CommandEmpty>
            <CommandGroup>
              {PROFILE_ICON_CHOICES.map((name) => (
                <CommandItem
                  key={name}
                  value={name}
                  onSelect={() => {
                    onChange(name);
                    setOpen(false);
                  }}
                >
                  <ProfileIcon name={name} className="mr-2 size-4" />
                  {name}
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
