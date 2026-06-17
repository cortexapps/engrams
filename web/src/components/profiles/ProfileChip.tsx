import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { HoverCard, HoverCardContent, HoverCardTrigger } from "@/components/ui/hover-card";
import { Badge } from "@/components/ui/badge";
import { ProfileIcon } from "./ProfileIcon";
import type { ProfileSnapshotView } from "@/lib/types";

/** Strip only the registry host from an OCI URI, keeping the repo path and tag.
 * e.g. `registry.io/repo/image:tag` → `repo/image:tag`. Used in the fallback
 * (profile-less) chip where the tag is meaningful identity. */
function stripRegistryHost(uri: string): string {
  const slash = uri.indexOf("/");
  return slash >= 0 ? uri.slice(slash + 1) : uri;
}

function Details({ p }: { p: ProfileSnapshotView }) {
  return (
    <div className="flex flex-col gap-1 text-sm">
      <div className="flex items-center gap-2 font-medium">
        <ProfileIcon name={p.icon} className="size-4" /> {p.name}
        {p.archived && <Badge variant="secondary">archived</Badge>}
      </div>
      <div className="font-mono text-xs text-muted-foreground">{p.imageUri}</div>
    </div>
  );
}

/**
 * App-wide profile identity chip (ADR §7). `disclosure="tooltip"` for dense link
 * rows (rail/list); `disclosure="hovercard"` for the detail header. Falls back to
 * the image string for legacy / profile-less sessions. The tooltip branch carries
 * its own TooltipProvider so the chip is safe to render outside the sidebar.
 */
export function ProfileChip({
  profile,
  fallbackImage,
  disclosure = "tooltip",
  className,
}: {
  profile: ProfileSnapshotView | null | undefined;
  fallbackImage?: string;
  disclosure?: "tooltip" | "hovercard";
  className?: string;
}) {
  if (!profile) {
    return (
      <span
        className={`min-w-0 truncate font-mono text-xs text-muted-foreground ${className ?? ""}`}
      >
        {fallbackImage ? stripRegistryHost(fallbackImage) : "—"}
      </span>
    );
  }

  const label = (
    <span className={`inline-flex min-w-0 items-center gap-1.5 ${className ?? ""}`}>
      <ProfileIcon name={profile.icon} className="size-3.5 shrink-0 text-muted-foreground" />
      <span className="truncate">{profile.name}</span>
      {profile.archived && (
        <Badge variant="secondary" className="px-1 py-0 text-[10px]">
          archived
        </Badge>
      )}
    </span>
  );

  if (disclosure === "hovercard") {
    return (
      <HoverCard openDelay={100}>
        <HoverCardTrigger asChild>
          <button type="button" className="text-left">
            {label}
          </button>
        </HoverCardTrigger>
        <HoverCardContent align="start">
          <Details p={profile} />
        </HoverCardContent>
      </HoverCard>
    );
  }
  return (
    <TooltipProvider delayDuration={100}>
      <Tooltip>
        <TooltipTrigger asChild>{label}</TooltipTrigger>
        <TooltipContent align="start">
          <Details p={profile} />
        </TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}
