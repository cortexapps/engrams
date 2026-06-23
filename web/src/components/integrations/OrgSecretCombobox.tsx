/**
 * OrgSecretCombobox — a styled typeahead over the org-secret names, replacing the
 * native `<datalist>` (which renders an unstyleable popup). Filters known secret
 * names and offers "Create new secret <value>" for an unknown ref (caution-marked
 * so the admin knows they're introducing one). Free-text: the committed value may
 * be an existing name or a new one. Only names cross the wire — never values.
 */

import { useState } from "react";
import { CheckIcon, ChevronsUpDownIcon, KeyRoundIcon, PlusIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Command,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { cn } from "@/lib/utils";

export interface OrgSecretComboboxProps {
  value: string;
  onChange: (ref: string) => void;
  secretNames: string[];
  placeholder?: string;
  id?: string;
  className?: string;
}

export function OrgSecretCombobox({
  value,
  onChange,
  secretNames,
  placeholder = "org-secret ref",
  id,
  className,
}: OrgSecretComboboxProps) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");

  const trimmed = query.trim();
  const q = trimmed.toLowerCase();
  const matches = secretNames.filter((n) => n.toLowerCase().includes(q));
  const showCreate = trimmed.length > 0 && !secretNames.includes(trimmed);

  const commit = (ref: string) => {
    onChange(ref);
    setQuery("");
    setOpen(false);
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          id={id}
          type="button"
          variant="outline"
          role="combobox"
          aria-expanded={open}
          className={cn(
            "h-9 w-full justify-between gap-2 font-mono text-xs font-normal",
            !value && "text-muted-foreground",
            className,
          )}
        >
          <span className="flex min-w-0 items-center gap-2">
            <KeyRoundIcon className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="truncate">{value || placeholder}</span>
          </span>
          <ChevronsUpDownIcon className="size-3.5 shrink-0 opacity-50" />
        </Button>
      </PopoverTrigger>
      <PopoverContent className="p-0" align="start">
        {/* Filtering is manual (we also offer a create-new item), so disable cmdk's. */}
        <Command shouldFilter={false}>
          <CommandInput placeholder="Search or create…" value={query} onValueChange={setQuery} />
          <CommandList>
            {matches.length === 0 && !showCreate && (
              <div className="py-6 text-center text-sm text-muted-foreground">
                No secrets match.
              </div>
            )}
            <CommandGroup>
              {matches.map((n) => (
                <CommandItem key={n} value={n} onSelect={() => commit(n)}>
                  <KeyRoundIcon className="mr-2 size-3.5 text-muted-foreground" />
                  <span className="font-mono text-xs">{n}</span>
                  {n === value && <CheckIcon className="ml-auto size-3.5 text-primary" />}
                </CommandItem>
              ))}
              {showCreate && (
                <CommandItem value={`create:${trimmed}`} onSelect={() => commit(trimmed)}>
                  <PlusIcon className="mr-2 size-3.5 text-instrument-caution" />
                  <span className="text-xs">
                    Create new secret <code className="font-mono">{trimmed}</code>
                  </span>
                </CommandItem>
              )}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
