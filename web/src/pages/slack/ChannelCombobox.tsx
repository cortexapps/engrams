/**
 * ChannelCombobox — a searchable picker over the channels the Slack app can
 * see ("#name", id beneath), for the override dialog. The list is the
 * provider's, so a channel it does not show (the app is not in it yet, or
 * the walk was capped) can still be added by its id: typing one offers
 * "Use channel ID …". The committed value is always the id.
 */
import { useState } from "react";
import { CheckIcon, ChevronsUpDownIcon, HashIcon } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
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

export interface ChannelOption {
  key: string;
  label: string;
}

/** A Slack conversation id as Slack writes it: C… public, G… private
 * (legacy), D… direct. */
export const SLACK_CHANNEL_ID_RE = /^[CGD][A-Z0-9]{6,}$/;

export interface ChannelComboboxProps {
  value: string;
  onChange: (channelId: string) => void;
  options: readonly ChannelOption[];
  loading?: boolean;
  disabled?: boolean;
  id?: string;
  className?: string;
}

export function ChannelCombobox({
  value,
  onChange,
  options,
  loading = false,
  disabled = false,
  id,
  className,
}: ChannelComboboxProps) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const trimmed = query.trim();
  const q = trimmed.toLowerCase();
  const matches = options.filter(
    (o) => o.label.toLowerCase().includes(q) || o.key.toLowerCase() === q,
  );
  const typedId = trimmed.toUpperCase();
  const offerId = SLACK_CHANNEL_ID_RE.test(typedId) && !options.some((o) => o.key === typedId);
  const selected = options.find((o) => o.key === value);
  const commit = (channelId: string) => {
    onChange(channelId);
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
          disabled={disabled}
          className={cn(
            "h-9 w-full justify-between gap-2 font-normal",
            !value && "text-muted-foreground",
            className,
          )}
        >
          <span className="flex min-w-0 items-center gap-2">
            <HashIcon className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="truncate">
              {selected ? selected.label : value ? value : "Pick a channel"}
            </span>
          </span>
          <ChevronsUpDownIcon className="size-3.5 shrink-0 opacity-50" />
        </Button>
      </PopoverTrigger>
      <PopoverContent className="p-0" align="start">
        {/* Filtering is manual (an id the list lacks is also offered), so cmdk's is off. */}
        <Command shouldFilter={false}>
          <CommandInput placeholder="Search channels…" value={query} onValueChange={setQuery} />
          <CommandList>
            {loading && options.length === 0 ? (
              <EmptyState inline className="px-3 py-6">
                Loading channels…
              </EmptyState>
            ) : (
              matches.length === 0 &&
              !offerId && (
                <EmptyState inline className="px-3 py-6">
                  No channels match. A channel the app is not in yet can be added by its ID.
                </EmptyState>
              )
            )}
            <CommandGroup>
              {matches.map((o) => (
                <CommandItem key={o.key} value={o.key} onSelect={() => commit(o.key)}>
                  <span className="flex min-w-0 flex-1 flex-col">
                    <span className="truncate text-sm">{o.label}</span>
                    <span className="truncate font-mono text-2xs text-muted-foreground">
                      {o.key}
                    </span>
                  </span>
                  {o.key === value && <CheckIcon className="ml-2 size-3.5 shrink-0 text-primary" />}
                </CommandItem>
              ))}
              {offerId && (
                <CommandItem value={`id:${typedId}`} onSelect={() => commit(typedId)}>
                  <span className="text-sm">
                    Use channel ID <code className="font-mono text-xs">{typedId}</code>
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
