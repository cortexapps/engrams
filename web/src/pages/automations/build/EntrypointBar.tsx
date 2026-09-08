/** Entrypoint switcher (ADR 0119 D9): one button per way into the automation.
 *
 * The Build tab edits one entrypoint at a time through a projection; this
 * bar owns which one. It renders only when the automation has (or is
 * gaining) extra entrypoints, so the classic single-entrypoint editor looks
 * exactly as before. Built-ins are structure-locked: no add, no remove.
 */

import { useState } from "react";
import { Plus, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import {
  entrypointIdError,
  entrypointIds,
  MAIN_ENTRYPOINT_ID,
  type AutomationDefinition,
} from "@/lib/automation-blocks";

export interface EntrypointBarProps {
  definition: AutomationDefinition;
  selected: string;
  onSelect: (id: string) => void;
  onAdd: (id: string) => void;
  onRemove: (id: string) => void;
  locked: boolean;
}

export function EntrypointBar({
  definition,
  selected,
  onSelect,
  onAdd,
  onRemove,
  locked,
}: EntrypointBarProps) {
  const ids = entrypointIds(definition);
  const [adding, setAdding] = useState(false);
  const [draftId, setDraftId] = useState("");
  const draftError = draftId === "" ? null : entrypointIdError(definition, draftId);

  if (ids.length === 1 && locked) return null;

  const confirmAdd = () => {
    if (draftId === "" || draftError !== null) return;
    onAdd(draftId);
    onSelect(draftId);
    setDraftId("");
    setAdding(false);
  };

  return (
    <div className="flex flex-wrap items-center gap-2" data-testid="entrypoint-bar">
      <span className="text-xs font-medium text-muted-foreground">Entrypoints</span>
      {ids.map((id) => (
        <span key={id} className="inline-flex items-center">
          <Button
            type="button"
            size="sm"
            variant={id === selected ? "default" : "outline"}
            className={cn("h-7 rounded-sm px-3 text-xs", id !== selected && "font-normal")}
            onClick={() => onSelect(id)}
            data-testid={`entrypoint-${id}`}
          >
            {id}
          </Button>
          {!locked && id !== MAIN_ENTRYPOINT_ID && id === selected && (
            <Button
              type="button"
              size="icon"
              variant="ghost"
              className="ml-0.5 size-6"
              aria-label={`Remove entrypoint ${id}`}
              onClick={() => {
                onRemove(id);
                onSelect(MAIN_ENTRYPOINT_ID);
              }}
            >
              <X className="size-3.5" aria-hidden />
            </Button>
          )}
        </span>
      ))}
      {!locked &&
        (adding ? (
          <span className="inline-flex items-center gap-1">
            <Input
              autoFocus
              value={draftId}
              onChange={(e) => setDraftId(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") confirmAdd();
                if (e.key === "Escape") {
                  setAdding(false);
                  setDraftId("");
                }
              }}
              placeholder="entrypoint_id"
              className="h-7 w-40 text-xs"
              aria-label="New entrypoint id"
              aria-invalid={draftError !== null}
              title={draftError ?? undefined}
            />
            <Button
              type="button"
              size="sm"
              className="h-7 px-2 text-xs"
              disabled={draftId === "" || draftError !== null}
              onClick={confirmAdd}
            >
              Add
            </Button>
          </span>
        ) : (
          <Button
            type="button"
            size="sm"
            variant="ghost"
            className="h-7 px-2 text-xs"
            onClick={() => setAdding(true)}
            data-testid="add-entrypoint"
          >
            <Plus className="size-3.5" aria-hidden /> Entrypoint
          </Button>
        ))}
    </div>
  );
}
