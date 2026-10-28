/** The "+" affordance on an edge: a small round button (32px invisible hit
 * area, revealed on hover of that area or keyboard focus) opening the block
 * kind menu. `aria-label="Insert block here"` is the BlockList contract the
 * editor tests rely on — kept verbatim. */

import { Plus } from "lucide-react";

import { DropdownMenu, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import type { ListPath } from "@/lib/automation-blocks";

import { BlockKindMenu } from "./BlockKindMenu";

export interface InsertMenuProps {
  x: number;
  y: number;
  at: ListPath;
  index: number;
  onInsert: (at: ListPath, index: number, kind: string) => void;
  withAgent?: boolean;
}

export function InsertMenu({ x, y, at, index, onInsert, withAgent }: InsertMenuProps) {
  return (
    <div
      className="group/insert absolute z-20 flex size-8 -translate-x-1/2 -translate-y-1/2 items-center justify-center"
      style={{ left: x, top: y }}
    >
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            aria-label="Insert block here"
            className="bg-card text-muted-foreground hover:text-foreground border-border hover:border-ring flex size-5 items-center justify-center rounded-full border opacity-0 group-hover/insert:opacity-100 focus-visible:opacity-100 data-[state=open]:opacity-100"
          >
            <Plus className="size-3.5" aria-hidden />
          </button>
        </DropdownMenuTrigger>
        <BlockKindMenu onPick={(kind) => onInsert(at, index, kind)} withAgent={withAgent} />
      </DropdownMenu>
    </div>
  );
}
