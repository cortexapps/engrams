/** Settings → Tags: every installation-wide tag, with its description and how
 * many automations use it. Admins create, edit and delete tags here. */

import { Link } from "@tanstack/react-router";
import { PlusIcon } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";

import { EmptyState } from "@/components/empty-state";
import { PageHeading } from "@/components/page-heading";
import { SkeletonRows } from "@/components/skeleton-rows";
import { randomTagColor } from "@/components/tags/colors";
import { TagChip } from "@/components/tags/TagChip";
import { TagColorField } from "@/components/tags/TagColorField";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { TagSummary } from "@/gen/engram/app/v1/tag_pb";
import { useCreateTag, useDeleteTag, useTags, useUpdateTag } from "@/hooks/useTags";
import { errorMessage } from "@/lib/errors";

interface TagFields {
  name: string;
  description: string;
  color: string;
}

function automationCount(n: number): string {
  return `${n} automation${n === 1 ? "" : "s"}`;
}

/** The one form for a new tag and for an edit. `onSave` resolves true when
 * the server accepted the tag. */
function TagForm({
  initial,
  onSave,
  onCancel,
}: {
  initial: TagFields;
  onSave: (fields: TagFields) => Promise<boolean>;
  onCancel: () => void;
}) {
  const [fields, setFields] = useState(initial);
  const [saving, setSaving] = useState(false);
  const set = (patch: Partial<TagFields>) => setFields((current) => ({ ...current, ...patch }));

  return (
    <form
      role="row"
      aria-label={initial.name ? `Edit ${initial.name}` : "New tag"}
      className="flex flex-col gap-3 px-4 py-3"
      onSubmit={async (event) => {
        event.preventDefault();
        setSaving(true);
        const saved = await onSave(fields);
        setSaving(false);
        if (saved) onCancel();
      }}
    >
      <div className="flex flex-wrap items-center gap-2">
        <Input
          aria-label="Tag name"
          placeholder="Name"
          value={fields.name}
          onChange={(event) => set({ name: event.target.value })}
          className="h-8 w-48"
          autoFocus
        />
        <Input
          aria-label="Tag description"
          placeholder="Description (optional)"
          value={fields.description}
          onChange={(event) => set({ description: event.target.value })}
          className="h-8 min-w-0 flex-1"
        />
      </div>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap items-center gap-3">
          <TagColorField value={fields.color} onChange={(color) => set({ color })} />
          <TagChip tag={{ ...fields, name: fields.name.trim() || "Preview" }} />
        </div>
        <div className="flex items-center gap-2">
          <Button type="button" variant="ghost" size="sm" onClick={onCancel}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={saving}>
            Save
          </Button>
        </div>
      </div>
    </form>
  );
}

export function Tags() {
  const tags = useTags();
  const createTag = useCreateTag();
  const updateTag = useUpdateTag();
  const deleteTag = useDeleteTag();
  const [search, setSearch] = useState("");
  /** The tag being edited, "new" for the new-tag form, or null. */
  const [editing, setEditing] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<TagSummary | null>(null);

  const all = tags.data?.tags ?? [];
  const needle = search.trim().toLowerCase();
  const shown = needle ? all.filter((s) => s.tag?.name.toLowerCase().includes(needle)) : all;

  const save = async (write: () => Promise<unknown>): Promise<boolean> => {
    try {
      await write();
      return true;
    } catch (error) {
      toast.error(errorMessage(error));
      return false;
    }
  };

  const confirmDelete = async () => {
    const id = deleting?.tag?.id;
    setDeleting(null);
    if (id) await save(() => deleteTag.mutateAsync({ id }));
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-6 overflow-auto p-4 md:p-6">
      <PageHeading
        title="Tags"
        count={tags.data ? String(all.length) : undefined}
        actions={
          <Button size="sm" onClick={() => setEditing("new")} disabled={editing === "new"}>
            <PlusIcon className="size-3.5" />
            New tag
          </Button>
        }
      />

      {tags.error ? (
        <EmptyState tone="error">Could not load the tags: {errorMessage(tags.error)}</EmptyState>
      ) : tags.isPending ? (
        <SkeletonRows rows={3} columns={["minmax(8rem,12rem)", "minmax(0,1fr)", "8rem"]} />
      ) : (
        <section aria-label="Tags" className="flex flex-col gap-3">
          {all.length > 0 && (
            <Input
              aria-label="Search tags"
              placeholder="Search tags…"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              className="max-w-xs"
            />
          )}
          {all.length === 0 && editing !== "new" ? (
            <EmptyState>
              No tags yet. Create one here, or from the tag picker on an automation.
            </EmptyState>
          ) : (
            <div role="table" aria-label="Tags" className="divide-y rounded-lg border bg-card">
              {editing === "new" && (
                <TagForm
                  initial={{ name: "", description: "", color: randomTagColor() }}
                  onSave={(fields) => save(() => createTag.mutateAsync(fields))}
                  onCancel={() => setEditing(null)}
                />
              )}
              {shown.map((summary) => {
                const tag = summary.tag;
                if (!tag) return null;
                if (editing === tag.id) {
                  return (
                    <TagForm
                      key={tag.id}
                      initial={{ name: tag.name, description: tag.description, color: tag.color }}
                      onSave={(fields) =>
                        save(() => updateTag.mutateAsync({ id: tag.id, ...fields }))
                      }
                      onCancel={() => setEditing(null)}
                    />
                  );
                }
                return (
                  <div
                    key={tag.id}
                    role="row"
                    className="grid grid-cols-[minmax(8rem,12rem)_minmax(0,1fr)_8rem_auto] items-center gap-x-4 px-4 py-2.5"
                  >
                    <span role="cell" className="min-w-0">
                      <TagChip tag={tag} />
                    </span>
                    <span role="cell" className="truncate text-sm text-muted-foreground">
                      {tag.description}
                    </span>
                    <span role="cell" className="text-sm">
                      <Link
                        to="/automations"
                        search={{ tags: [tag.name] }}
                        className="tabular-nums hover:underline"
                      >
                        {automationCount(summary.automationCount)}
                      </Link>
                    </span>
                    <span role="cell" className="flex items-center gap-1">
                      <Button
                        variant="ghost"
                        size="sm"
                        aria-label={`Edit ${tag.name}`}
                        onClick={() => setEditing(tag.id)}
                      >
                        Edit
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        aria-label={`Delete ${tag.name}`}
                        onClick={() => setDeleting(summary)}
                      >
                        Delete
                      </Button>
                    </span>
                  </div>
                );
              })}
            </div>
          )}
        </section>
      )}

      <AlertDialog open={deleting !== null} onOpenChange={(open) => !open && setDeleting(null)}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Delete "{deleting?.tag?.name}"?</AlertDialogTitle>
            <AlertDialogDescription>
              It is removed from {automationCount(deleting?.automationCount ?? 0)}.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction variant="destructive" onClick={() => void confirmDelete()}>
              Delete
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
