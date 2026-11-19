/** The editor's tag control: a searchable list of every tag, where checking
 * a tag adds it to the automation and unchecking removes it. A name that
 * matches no tag can be created on the spot, with a random color. */

import { Check, Pencil, Plus } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import type { Tag } from "@/gen/engram/app/v1/tag_pb";
import { useAddAutomationTag, useRemoveAutomationTag } from "@/hooks/useAutomationEditor";
import { useCreateTag, useTags } from "@/hooks/useTags";
import { errorMessage } from "@/lib/errors";
import { randomTagColor } from "./colors";
import { TagChip } from "./TagChip";

type TagView = Pick<Tag, "id" | "name" | "color" | "description">;

export function TagPicker({
  automationId,
  tags,
}: {
  automationId: string;
  tags: readonly TagView[];
}) {
  const allTags = useTags();
  const addTag = useAddAutomationTag();
  const removeTag = useRemoveAutomationTag();
  const createTag = useCreateTag();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");

  const own = new Set(tags.map((tag) => tag.id));
  const others = (allTags.data?.tags ?? []).flatMap((summary) =>
    summary.tag && !own.has(summary.tag.id) ? [summary.tag] : [],
  );
  const typed = query.trim();
  const nameExists = [...tags, ...others].some(
    (tag) => tag.name.toLowerCase() === typed.toLowerCase(),
  );

  const run = async (write: () => Promise<unknown>) => {
    try {
      await write();
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  const toggle = (tag: TagView) =>
    run(() =>
      own.has(tag.id)
        ? removeTag.mutateAsync({ automationId, tagId: tag.id })
        : addTag.mutateAsync({ automationId, tagId: tag.id }),
    );

  const createAndAdd = () =>
    run(async () => {
      const { tag } = await createTag.mutateAsync({
        name: typed,
        color: randomTagColor(),
        description: "",
      });
      if (!tag) throw new Error("the server returned no tag");
      await addTag.mutateAsync({ automationId, tagId: tag.id });
      setQuery("");
    });

  const option = (tag: TagView) => (
    // The search scores the value, so it holds the name only (names are
    // unique); an id in it would match unrelated searches.
    <CommandItem key={tag.id} value={tag.name} onSelect={() => void toggle(tag)}>
      <Check className={own.has(tag.id) ? "opacity-100" : "opacity-0"} aria-hidden />
      <TagChip tag={tag} />
      {own.has(tag.id) && <span className="sr-only">added</span>}
      {tag.description && (
        <span className="min-w-0 truncate text-xs text-muted-foreground">{tag.description}</span>
      )}
    </CommandItem>
  );

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) setQuery("");
      }}
    >
      <PopoverTrigger asChild>
        <Button type="button" variant="ghost" size="xs">
          {tags.length > 0 ? <Pencil /> : <Plus />}
          {tags.length > 0 ? "Edit tags" : "Add tag"}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-72 p-0">
        <Command label="Search tags">
          <CommandInput
            placeholder="Search or create a tag…"
            value={query}
            onValueChange={setQuery}
          />
          <CommandList>
            <CommandEmpty>No tags found.</CommandEmpty>
            <CommandGroup>
              {tags.map(option)}
              {others.map(option)}
            </CommandGroup>
            {/* Outside the group: cmdk hides a group whose items are all
                filtered out, and this item must show exactly then. */}
            {typed !== "" && !nameExists && (
              <CommandItem
                forceMount
                value={`create ${typed}`}
                onSelect={() => void createAndAdd()}
              >
                <Plus aria-hidden />
                Create tag "{typed}"
              </CommandItem>
            )}
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
