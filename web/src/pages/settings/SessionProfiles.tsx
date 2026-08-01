/**
 * Profiles list (redesign) — each profile is a wide clickable row: icon chip,
 * name + description, and a meta strip of provider tiles + power/write counts +
 * reachable-host count + image, derived from the same policy compile the editor
 * shows. Archived profiles collapse below.
 */

import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  ArchiveIcon,
  BoxIcon,
  ChevronRightIcon,
  GlobeIcon,
  PencilIcon,
  PlusIcon,
  ZapIcon,
} from "lucide-react";

import { useProfiles, useDeleteProfile } from "../../hooks/useProfiles";
import { useEnabledImages } from "../../hooks/useEnabledImages";
import { useHarnessCatalog } from "../../hooks/useHarnessCatalog";
import {
  useConnectorViews,
  type ConnectorView,
} from "../../components/integrations/useConnectorViews";
import { ProfileIcon } from "../../components/profiles/ProfileIcon";
import { ProviderTile } from "../../components/integrations/ProviderTile";
import { derivePolicy } from "../../lib/profilePolicy";
import { defaultCapabilitiesForGrants } from "../../lib/profileIntegrations";
import { PageHeading } from "../../components/page-heading";
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

interface ProfileLike {
  id: string;
  name: string;
  description: string;
  icon: string;
  imageId: string;
  archived: boolean;
  integrationGrants: Array<{
    connectionId: string;
    operation: string;
    resourceConstraints: string[];
  }>;
  network?: { default: string; allowHosts: string[]; allowHostPatterns: string[] };
  /** Default harness (catalog name) — resolves the harness's own egress hosts. */
  harness?: string | null;
}

function Meta({ icon, text, caution }: { icon: React.ReactNode; text: string; caution?: boolean }) {
  return (
    <span
      className={`inline-flex items-center gap-1 text-[0.74rem] ${caution ? "text-instrument-caution" : "text-muted-foreground"}`}
    >
      {icon}
      {text}
    </span>
  );
}

function Row({
  p,
  views,
  imageName,
  onArchive,
}: {
  p: ProfileLike;
  views: ConnectorView[];
  imageName: string;
  onArchive?: (id: string) => void;
}) {
  // ADR 0063 addendum: the profile's harness opens its own model-API hosts
  // (merged server-side at create); include them so the card's reach count
  // matches what a session actually gets. The query is deduped across rows.
  const { data: harnesses } = useHarnessCatalog(true);
  const harnessEgress = harnesses?.find((h) => h.name === p.harness)?.descriptor?.egress;
  const policy = derivePolicy(
    {
      capabilities: defaultCapabilitiesForGrants(p.integrationGrants ?? [], views),
      network: {
        default: p.network?.default === "allow" ? "allow" : "deny",
        allowHosts: p.network?.allowHosts ?? [],
        allowHostPatterns: p.network?.allowHostPatterns ?? [],
      },
      secrets: [],
    },
    views,
    {
      allowHosts: harnessEgress?.allowHosts ?? [],
      allowHostPatterns: harnessEgress?.allowHostPatterns ?? [],
    },
  );

  const inner = (
    <>
      <span className="flex size-11 shrink-0 items-center justify-center rounded-md bg-secondary text-foreground">
        <ProfileIcon name={p.icon} className="size-5" />
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="font-display text-base font-semibold">{p.name}</span>
          {p.archived && <Badge variant="secondary">archived</Badge>}
        </div>
        <div className="truncate text-[0.82rem] text-muted-foreground">{p.description}</div>
        <div className="mt-2 flex flex-wrap items-center gap-3.5">
          {policy.providers.length > 0 ? (
            <span className="inline-flex items-center gap-1">
              {policy.providers.map((pr) => (
                <ProviderTile
                  key={pr.view.provider}
                  {...pr.view.icon}
                  name={pr.view.name}
                  size={18}
                />
              ))}
            </span>
          ) : (
            <span className="text-[0.74rem] text-muted-foreground">no integrations</span>
          )}
          <Meta
            icon={<ZapIcon className="size-3 opacity-75" />}
            text={`${policy.capCount} power${policy.capCount === 1 ? "" : "s"}`}
          />
          {policy.writeCount > 0 && (
            <Meta
              icon={<PencilIcon className="size-3" />}
              text={`${policy.writeCount} write`}
              caution
            />
          )}
          <Meta
            icon={<GlobeIcon className="size-3 opacity-75" />}
            text={
              policy.reachable.length === 0
                ? "fully sandboxed"
                : `${policy.reachable.length} host${policy.reachable.length === 1 ? "" : "s"}`
            }
          />
          <Meta icon={<BoxIcon className="size-3 opacity-75" />} text={imageName} />
        </div>
      </div>
    </>
  );

  return (
    <div
      className="flex items-center gap-4 rounded-lg border bg-card p-4 shadow-xs"
      data-testid={`profile-${p.id}`}
    >
      {p.archived ? (
        <div className="flex flex-1 items-center gap-4">{inner}</div>
      ) : (
        <Link
          to="/settings/profiles/$id"
          params={{ id: p.id }}
          className="flex flex-1 items-center gap-4"
        >
          {inner}
        </Link>
      )}
      {p.archived ? (
        <span className="text-xs text-muted-foreground">archived</span>
      ) : (
        <div className="flex items-center gap-1">
          <Button asChild variant="ghost" size="sm">
            <Link to="/settings/profiles/$id" params={{ id: p.id }}>
              <PencilIcon className="size-4" />
              Edit
            </Link>
          </Button>
          {onArchive && (
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button variant="ghost" size="sm">
                  <ArchiveIcon className="size-4" />
                  Archive
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
          <ChevronRightIcon className="size-4 text-muted-foreground" />
        </div>
      )}
    </div>
  );
}

export function SessionProfiles() {
  const { data, isPending } = useProfiles(false);
  const { data: allData } = useProfiles(true);
  const { data: images } = useEnabledImages(true);
  const { views } = useConnectorViews();
  const del = useDeleteProfile();
  const [showArchived, setShowArchived] = useState(false);

  const active = data?.profiles ?? [];
  const archived = (allData?.profiles ?? []).filter((p) => p.archived);
  const imageName = (id: string) => images?.find((i) => i.id === id)?.image_uri ?? "—";

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
      <PageHeading
        title="Profiles"
        eyebrow="Org · Session starting points"
        description="A profile is a ready-made launch: an image plus the powers, network, and credentials its sessions get. Developers pick one and go."
        actions={
          <Button asChild size="sm">
            <Link to="/settings/profiles/new">
              <PlusIcon className="size-3.5" />
              New profile
            </Link>
          </Button>
        }
      />

      {isPending && <p className="text-sm text-muted-foreground">Loading…</p>}
      {!isPending && active.length === 0 && (
        <div className="rounded-lg border border-dashed p-8 text-center">
          <p className="text-sm text-muted-foreground">No profiles yet</p>
          <Button asChild className="mt-3">
            <Link to="/settings/profiles/new">Create profile</Link>
          </Button>
        </div>
      )}

      <div className="flex flex-col gap-3">
        {active.map((p) => (
          <Row
            key={p.id}
            p={p}
            views={views}
            imageName={imageName(p.imageId)}
            onArchive={onArchive}
          />
        ))}
      </div>

      {archived.length > 0 && (
        <Collapsible open={showArchived} onOpenChange={setShowArchived}>
          <CollapsibleTrigger asChild>
            <Button variant="ghost" size="sm" className="self-start">
              {showArchived ? "Hide" : "Show"} archived ({archived.length})
            </Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="mt-3 flex flex-col gap-3">
            {archived.map((p) => (
              <Row key={p.id} p={p} views={views} imageName={imageName(p.imageId)} />
            ))}
          </CollapsibleContent>
        </Collapsible>
      )}
    </div>
  );
}
