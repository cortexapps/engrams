/** The block kind menu behind every "Insert block here" affordance: the
 * insertable kinds by purpose (Agent, Flow, Sessions, Integrations), with the
 * engine and built-in internals one level down under Advanced. One component
 * so the "+" on an edge and the "Add step" tail offer the same list. */

import {
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
} from "@/components/ui/dropdown-menu";
import { useConnectedRouters } from "@/hooks/useConnectedRouters";
import { groupedBlockKinds, type BlockKindSpec } from "@/lib/automation-blocks";

export interface BlockKindMenuProps {
  onPick: (kind: string) => void;
  align?: "start" | "center" | "end";
  /** The automation has an agent: no wait blocks (see `groupedBlockKinds`). */
  withAgent?: boolean;
}

function KindItem({ spec, onPick }: { spec: BlockKindSpec; onPick: (kind: string) => void }) {
  return (
    <DropdownMenuItem onSelect={() => onPick(spec.kind)} title={spec.description}>
      <spec.icon className="size-4" aria-hidden />
      {spec.label}
    </DropdownMenuItem>
  );
}

export function BlockKindMenu({ onPick, align = "center", withAgent = false }: BlockKindMenuProps) {
  const connectedRouters = useConnectedRouters();
  const groups = groupedBlockKinds(connectedRouters, { withAgent });
  const advanced = groups.find((group) => group.id === "advanced");
  const plain = groups.filter((group) => group.id !== "advanced");
  return (
    <DropdownMenuContent align={align} className="min-w-44" data-testid="block-kind-menu">
      {plain.map((group, index) => (
        <DropdownMenuGroup key={group.id}>
          {index > 0 && <DropdownMenuSeparator />}
          <DropdownMenuLabel className="text-2xs font-medium tracking-wide text-muted-foreground uppercase">
            {group.label}
          </DropdownMenuLabel>
          {group.kinds.map((spec) => (
            <KindItem key={spec.kind} spec={spec} onPick={onPick} />
          ))}
        </DropdownMenuGroup>
      ))}
      {advanced && (
        <>
          <DropdownMenuSeparator />
          <DropdownMenuSub>
            <DropdownMenuSubTrigger>{advanced.label}</DropdownMenuSubTrigger>
            <DropdownMenuSubContent className="max-h-80 overflow-y-auto">
              {advanced.kinds.map((spec) => (
                <KindItem key={spec.kind} spec={spec} onPick={onPick} />
              ))}
            </DropdownMenuSubContent>
          </DropdownMenuSub>
        </>
      )}
    </DropdownMenuContent>
  );
}
