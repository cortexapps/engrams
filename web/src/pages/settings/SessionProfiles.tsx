import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { toast } from "sonner";
import { IdCard, Pencil, Archive } from "lucide-react";
import { useProfiles, useDeleteProfile } from "../../hooks/useProfiles";
import { ProfileIcon } from "../../components/profiles/ProfileIcon";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";

type ProfileRow = {
  id: string;
  name: string;
  description: string;
  icon: string;
  includeUserTokens: boolean;
  archived: boolean;
};

function Row({ p, onArchive }: { p: ProfileRow; onArchive?: (id: string) => void }) {
  return (
    <div className="flex items-center gap-3 rounded-md border p-3" data-testid={`profile-${p.id}`}>
      <ProfileIcon name={p.icon} className="size-5 shrink-0 text-muted-foreground" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="font-medium">{p.name}</span>
          {p.archived && <Badge variant="secondary">archived</Badge>}
        </div>
        <p className="truncate text-sm text-muted-foreground">{p.description}</p>
        {p.includeUserTokens && <p className="text-xs text-muted-foreground">carries your token</p>}
      </div>
      {!p.archived && (
        <div className="flex items-center gap-1">
          <Button asChild variant="ghost" size="sm">
            <Link to="/settings/profiles/$id" params={{ id: p.id }}>
              <Pencil className="size-4" /> Edit
            </Link>
          </Button>
          {onArchive && (
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button variant="ghost" size="sm">
                  <Archive className="size-4" /> Archive
                </Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Archive "{p.name}"?</AlertDialogTitle>
                  <AlertDialogDescription>
                    New sessions can no longer be started from it. Existing sessions and their
                    history are unaffected.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction onClick={() => onArchive(p.id)}>
                    Archive profile
                  </AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          )}
        </div>
      )}
    </div>
  );
}

export function SessionProfiles() {
  const { data, isPending } = useProfiles(false);
  const { data: allData } = useProfiles(true);
  const del = useDeleteProfile();
  const [showArchived, setShowArchived] = useState(false);

  const active = data?.profiles ?? [];
  const archived = (allData?.profiles ?? []).filter((p) => p.archived);

  const onArchive = async (id: string) => {
    try {
      await del.mutateAsync({ id });
      toast.success("Profile archived");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "Failed to archive profile");
    }
  };

  return (
    <div className="flex flex-col gap-6">
      <header className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <IdCard className="size-5" />
          <h1 className="text-lg font-semibold">Profiles</h1>
        </div>
        <Button asChild>
          <Link to="/settings/profiles/new">Create profile</Link>
        </Button>
      </header>

      {isPending && <p className="text-sm text-muted-foreground">Loading…</p>}
      {!isPending && active.length === 0 && (
        <div className="rounded-md border border-dashed p-8 text-center">
          <p className="text-sm text-muted-foreground">No profiles yet</p>
          <Button asChild className="mt-3">
            <Link to="/settings/profiles/new">Create profile</Link>
          </Button>
        </div>
      )}

      <div className="flex flex-col gap-2">
        {active.map((p) => (
          <Row key={p.id} p={p} onArchive={onArchive} />
        ))}
      </div>

      {archived.length > 0 && (
        <Collapsible open={showArchived} onOpenChange={setShowArchived}>
          <CollapsibleTrigger asChild>
            <Button variant="ghost" size="sm" className="self-start">
              {showArchived ? "Hide" : "Show"} archived ({archived.length})
            </Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="mt-2 flex flex-col gap-2">
            {archived.map((p) => (
              <Row key={p.id} p={p} />
            ))}
          </CollapsibleContent>
        </Collapsible>
      )}
    </div>
  );
}
