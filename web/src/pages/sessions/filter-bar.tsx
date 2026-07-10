import { useState, type ReactNode } from "react";
import { Check, ListFilter, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { cn } from "@/lib/utils";

// A controlled, config-driven Linear-style cascading-menu composite:
// the fields menu opens hover flyouts of values, while applied-field pills
// edit their field directly. Selection state stays with the caller via value/onChange.

export interface FilterOption {
  value: string;
  label: string;
  icon?: ReactNode;
}

export interface FilterField {
  key: string;
  label: string;
  icon?: ReactNode;
  options: FilterOption[];
}

interface FieldOptionListProps {
  field: FilterField;
  selectedValues: string[];
  onToggle: (optionValue: string) => void;
}

function FieldOptionList({ field, selectedValues, onToggle }: FieldOptionListProps) {
  return (
    <Command
      onKeyDown={(event) => {
        if (event.key !== "Escape") event.stopPropagation();
      }}
    >
      <CommandInput autoFocus placeholder="Filter…" aria-label={`Filter ${field.label} options`} />
      <CommandList>
        <CommandEmpty>No options found.</CommandEmpty>
        <CommandGroup>
          {field.options.map((option) => {
            const selected = selectedValues.includes(option.value);

            return (
              <CommandItem
                key={option.value}
                value={`${option.label} ${option.value}`}
                onSelect={() => onToggle(option.value)}
              >
                <span className="flex size-4 items-center justify-center rounded border">
                  <Check className={cn("size-3", !selected && "opacity-0")} />
                </span>
                {option.icon != null && (
                  <span aria-hidden="true" className="text-[0.7rem] leading-none">
                    {option.icon}
                  </span>
                )}
                <span>{option.label}</span>
              </CommandItem>
            );
          })}
        </CommandGroup>
      </CommandList>
    </Command>
  );
}

export function FilterBar({
  fields,
  value,
  onChange,
}: {
  fields: FilterField[];
  value: Record<string, string[]>;
  onChange: (next: Record<string, string[]>) => void;
}) {
  const [openTarget, setOpenTarget] = useState<string | null>(null);
  const [openSubmenu, setOpenSubmenu] = useState<string | null>(null);

  const setSurfaceOpen = (target: string) => (open: boolean) => {
    setOpenTarget((current) => (open ? target : current === target ? null : current));
    if (target === "trigger") setOpenSubmenu(null);
  };

  const toggleOption = (field: FilterField, optionValue: string) => {
    const selectedValues = value[field.key] ?? [];
    const nextValues = selectedValues.includes(optionValue)
      ? selectedValues.filter((candidate) => candidate !== optionValue)
      : [...selectedValues, optionValue];
    onChange({ ...value, [field.key]: nextValues });
  };

  return (
    <div className="flex flex-wrap items-center gap-2">
      {fields.map((field) => {
        const selectedValues = value[field.key] ?? [];
        if (selectedValues.length === 0) return null;
        const firstOption = field.options.find((option) => option.value === selectedValues[0]);
        const target = `field:${field.key}`;

        return (
          <div
            key={field.key}
            role="group"
            aria-label={`${field.label} filter`}
            className="inline-flex h-7 items-stretch overflow-hidden rounded-full border bg-background text-xs text-muted-foreground shadow-xs"
          >
            <Popover open={openTarget === target} onOpenChange={setSurfaceOpen(target)}>
              <PopoverTrigger asChild>
                <button
                  type="button"
                  className="px-2 transition-colors outline-none hover:bg-accent hover:text-accent-foreground focus-visible:bg-accent focus-visible:text-accent-foreground"
                >
                  <span className="font-medium text-foreground">{field.label}:</span>{" "}
                  {firstOption?.label ?? selectedValues[0]}
                  {selectedValues.length > 1 && ` +${selectedValues.length - 1}`}
                </button>
              </PopoverTrigger>
              <PopoverContent align="start" className="w-64 p-0">
                <FieldOptionList
                  field={field}
                  selectedValues={selectedValues}
                  onToggle={(optionValue) => toggleOption(field, optionValue)}
                />
              </PopoverContent>
            </Popover>
            <button
              type="button"
              aria-label={`Clear ${field.label} filter`}
              className="flex items-center border-l px-1.5 transition-colors outline-none hover:bg-accent hover:text-accent-foreground focus-visible:bg-accent focus-visible:text-accent-foreground"
              onClick={() => {
                onChange({ ...value, [field.key]: [] });
                setOpenTarget((current) => (current === target ? null : current));
              }}
            >
              <X className="size-3" />
            </button>
          </div>
        );
      })}

      <DropdownMenu open={openTarget === "trigger"} onOpenChange={setSurfaceOpen("trigger")}>
        <DropdownMenuTrigger asChild>
          <Button variant="outline" size="sm">
            <ListFilter />
            Filter
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start">
          {fields.map((field) => {
            const selectedValues = value[field.key] ?? [];

            return (
              <DropdownMenuSub
                key={field.key}
                open={openSubmenu === field.key}
                onOpenChange={(open) => {
                  setOpenSubmenu((current) =>
                    open ? field.key : current === field.key ? null : current,
                  );
                }}
              >
                <DropdownMenuSubTrigger>
                  {field.icon != null && <span aria-hidden="true">{field.icon}</span>}
                  <span>{field.label}</span>
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent className="w-64 p-0">
                  <FieldOptionList
                    field={field}
                    selectedValues={selectedValues}
                    onToggle={(optionValue) => toggleOption(field, optionValue)}
                  />
                </DropdownMenuSubContent>
              </DropdownMenuSub>
            );
          })}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
