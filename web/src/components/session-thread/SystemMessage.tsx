import { useAuiState } from "@assistant-ui/react";
import {
  CameraIcon,
  DownloadIcon,
  ExternalLinkIcon,
  InfoIcon,
  RotateCcwIcon,
  XIcon,
} from "lucide-react";
import { useState } from "react";
import { Card, CardContent } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Text } from "@/components/ui/text";
import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog";
import { ProviderTile } from "@/components/integrations/ProviderTile";
import { useProviderIdentity } from "../../hooks/useIntegrations";
import { API_BASE } from "../../lib/base";
import { fmtBytes, hms } from "../transcriptFmt";
import type { SystemMarker } from "./buildMessages";
import { UserQuestionCard } from "./UserQuestionCard";

// The "harness register": non-message timeline events (durability markers,
// opened PRs, shared artifacts, system notes) carried as system messages.
// Visually distinct from the agent's tool calls — durability/notes are faint
// centered markers; PRs and artifacts are framed cards. Switches on the
// marker payload stashed in metadata.custom by buildMessages.

export function SystemMessage() {
  const marker = useAuiState((s) => s.message.metadata.custom?.marker as SystemMarker | undefined);
  const fallback = useAuiState((s) => {
    const part = s.message.content[0];
    return part?.type === "text" ? part.text : "";
  });

  if (!marker) return <Note text={fallback} />;

  switch (marker.kind) {
    case "durability":
      return <Durability marker={marker} />;
    case "integration_asset":
      return <IntegrationAsset marker={marker} />;
    case "artifact":
      return <Artifact marker={marker} />;
    case "recovery":
      return <Recovery marker={marker} />;
    case "user_question":
      return <UserQuestionCard marker={marker} />;
    case "note":
      return <Note text={fallback} />;
  }
}

// ADR 0028 A.log: the honest recovery boundary. Everything above (greyed)
// was rolled back by a rung-1 recovery; the thread resumes below. Surviving
// outside-world side effects are called out — the platform can't undo them.
// ADR 0045 F1: a planned operator relocation (drain / teleport) rewinds the
// same way but no host failed, so the copy must not cry "host failure".
function Recovery({ marker }: { marker: Extract<SystemMarker, { kind: "recovery" }> }) {
  return (
    <Card className="border-primary/40 bg-primary/5 py-0">
      <CardContent className="flex flex-col gap-1.5 p-4">
        <div className="flex items-center gap-2 text-xs text-primary">
          <RotateCcwIcon className="size-3.5" />
          <Text as="span" variant="label">
            {marker.planned
              ? "relocated to a new host"
              : "recovered from a checkpoint after a host failure"}
          </Text>
          <span className="ml-auto font-mono tabular-nums text-muted-foreground">
            {hms(marker.at)}
          </span>
        </div>
        <p className="text-sm text-muted-foreground">
          ~{marker.rolledBack} {marker.rolledBack === 1 ? "event" : "events"} after this point were
          rolled back{marker.planned ? " to the last checkpoint during the move" : ""}; the agent
          resumed from here.
        </p>
        {marker.survivingSideEffects.length > 0 && (
          <ul className="list-disc pl-5 text-sm text-muted-foreground">
            {marker.survivingSideEffects.map((s, i) => (
              <li key={i}>{s}</li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}

function Durability({ marker }: { marker: Extract<SystemMarker, { kind: "durability" }> }) {
  const Icon = marker.mark === "snapshot" ? CameraIcon : RotateCcwIcon;
  const label = marker.mark === "snapshot" ? "snapshotted" : "resumed";
  return (
    <div className="flex items-center justify-center gap-2 py-1 text-xs text-muted-foreground">
      <Icon className="size-3.5" />
      <Text as="span" variant="label">
        {label}
      </Text>
      {marker.sizeBytes != null && (
        <>
          <span aria-hidden>·</span>
          <span className="tabular-nums">{fmtBytes(marker.sizeBytes)}</span>
        </>
      )}
      <span aria-hidden>·</span>
      <span className="font-mono tabular-nums">{hms(marker.at)}</span>
    </div>
  );
}

// ADR 0056: the wire carries only semantic data (provider, asset_kind, data,
// fetchable) — never how to draw it. The web owns the
// (provider, asset_kind) → renderer registry; a never-seen pair falls back to
// a generic card so a new integration always surfaces *something* with no web
// code. engrams-provided shapes are hand-crafted here; a future plugin layer
// would register render treatment in this same layer, not on the wire.
type IntegrationAssetMarker = Extract<SystemMarker, { kind: "integration_asset" }>;

function IntegrationAsset({ marker }: { marker: IntegrationAssetMarker }) {
  // pull_request is GitHub-shaped regardless of the provider id minting it
  // (the legacy "forge" name and the current "github" both map here).
  if (marker.assetKind === "pull_request") return <PullRequestCard marker={marker} />;
  return <GenericAsset marker={marker} />;
}

// Typed-but-opaque payload accessors — `data` is provider-shaped JSON.
function dataStr(data: Record<string, unknown>, key: string): string | undefined {
  const v = data[key];
  return typeof v === "string" ? v : undefined;
}
function dataNum(data: Record<string, unknown>, key: string): number | undefined {
  const v = data[key];
  return typeof v === "number" ? v : undefined;
}

// Built-in renderer for `forge/pull_request` — the card the old
// PullRequestOpened event rendered, now fed from `data` + `fetchable`.
function PullRequestCard({ marker }: { marker: IntegrationAssetMarker }) {
  const identity = useProviderIdentity(marker.provider);
  const url = marker.fetchable?.kind === "external" ? marker.fetchable.url : undefined;
  const repo = dataStr(marker.data, "repo");
  const title = dataStr(marker.data, "title") ?? "pull request";
  const number = dataNum(marker.data, "number");
  const headBranch = dataStr(marker.data, "head_branch");
  const baseBranch = dataStr(marker.data, "base_branch");
  return (
    <Card className="py-0">
      <CardContent className="flex flex-col gap-1.5 p-4">
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <ProviderTile {...identity.icon} name={identity.name} size={16} />
          <Text as="span" variant="label">
            pull request
          </Text>
          {repo != null && number != null && (
            <>
              <span aria-hidden>·</span>
              <span className="font-mono">
                {repo} #{number}
              </span>
            </>
          )}
          <span className="ml-auto font-mono tabular-nums">{hms(marker.at)}</span>
        </div>
        {url ? (
          <a
            href={url}
            target="_blank"
            rel="noreferrer"
            className="group inline-flex items-baseline gap-1 text-base font-medium hover:underline"
          >
            {title}
            <ExternalLinkIcon className="size-3.5 shrink-0 self-center text-muted-foreground" />
          </a>
        ) : (
          <span className="text-base font-medium">{title}</span>
        )}
        {headBranch != null && baseBranch != null && (
          <div className="flex items-center gap-1.5 font-mono text-xs text-muted-foreground">
            <Badge variant="outline" className="font-normal">
              {headBranch}
            </Badge>
            <span aria-hidden>→</span>
            <Badge variant="outline" className="font-normal">
              {baseBranch}
            </Badge>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

// Generic fallback for any (provider, asset_kind) without a hand-crafted
// renderer: a header + a key/value dump of the scalar `data` fields + a
// fetchable link. No per-provider code — a polished card is an opt-in entry
// in the registry above.
function GenericAsset({ marker }: { marker: IntegrationAssetMarker }) {
  const identity = useProviderIdentity(marker.provider);
  const url = marker.fetchable?.kind === "external" ? marker.fetchable.url : undefined;
  const entries = Object.entries(marker.data).filter(
    ([, v]) => typeof v === "string" || typeof v === "number" || typeof v === "boolean",
  );
  return (
    <Card className="py-0">
      <CardContent className="flex flex-col gap-1.5 p-4">
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <ProviderTile {...identity.icon} name={identity.name} size={16} />
          <Text as="span" variant="label">
            {identity.name}
          </Text>
          <span aria-hidden>·</span>
          <span className="font-mono">{marker.assetKind}</span>
          <span className="ml-auto font-mono tabular-nums">{hms(marker.at)}</span>
        </div>
        {entries.length > 0 && (
          <div className="flex flex-col gap-0.5 text-sm">
            {entries.map(([k, v]) => (
              <div key={k} className="flex gap-2">
                <span className="shrink-0 font-mono text-xs text-muted-foreground">{k}</span>
                <span className="truncate">{String(v)}</span>
              </div>
            ))}
          </div>
        )}
        {url && (
          <a
            href={url}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1 text-sm hover:underline"
          >
            <ExternalLinkIcon className="size-3.5" /> Open
          </a>
        )}
      </CardContent>
    </Card>
  );
}

function Artifact({ marker }: { marker: Extract<SystemMarker, { kind: "artifact" }> }) {
  // Same-origin GET; the browser carries the auth cookie / dev proxy. No
  // bearer needed for a passive <img>/<video>.
  const src = `${API_BASE}/sessions/${marker.sessionId}/artifacts/${marker.artifactId}`;
  const isImage = marker.mediaType.startsWith("image/");
  const isVideo = marker.mediaType.startsWith("video/");
  const [lightboxOpen, setLightboxOpen] = useState(false);

  return (
    <Card className="overflow-hidden py-0">
      <CardContent className="flex flex-col gap-2 p-4">
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <DownloadIcon className="size-3.5 text-primary" />
          <Text as="span" variant="label">
            shared file
          </Text>
          <span aria-hidden>·</span>
          <span className="font-mono">{marker.mediaType}</span>
          <span aria-hidden>·</span>
          <span className="tabular-nums">{fmtBytes(marker.sizeBytes)}</span>
          <span className="ml-auto font-mono tabular-nums">{hms(marker.at)}</span>
        </div>

        {isImage ? (
          <>
            <img
              src={src}
              alt={marker.caption ?? "shared image"}
              className="w-auto max-h-[32rem] max-w-full self-start rounded-md border cursor-zoom-in"
              onClick={() => setLightboxOpen(true)}
            />
            <Dialog open={lightboxOpen} onOpenChange={setLightboxOpen}>
              <DialogContent
                showCloseButton={false}
                className="max-w-[min(95vw,900px)] p-0 gap-0 flex flex-col overflow-hidden"
              >
                <div className="flex shrink-0 items-center justify-between border-b px-3 py-2">
                  <a
                    href={src}
                    target="_blank"
                    rel="noreferrer"
                    className="flex items-center gap-1.5 text-xs text-muted-foreground transition-colors hover:text-foreground"
                    onClick={(e) => e.stopPropagation()}
                  >
                    <ExternalLinkIcon className="size-3.5" />
                    Open in new tab
                  </a>
                  <DialogClose asChild>
                    <Button variant="ghost" size="icon" className="size-7">
                      <XIcon className="size-4" />
                      <span className="sr-only">Close</span>
                    </Button>
                  </DialogClose>
                </div>
                <div className="overflow-y-auto">
                  <img src={src} alt={marker.caption ?? "shared image"} className="w-full" />
                </div>
                {marker.caption && (
                  <p className="shrink-0 border-t px-3 py-2 text-sm">{marker.caption}</p>
                )}
              </DialogContent>
            </Dialog>
          </>
        ) : isVideo ? (
          // eslint-disable-next-line jsx-a11y/media-has-caption
          <video
            src={src}
            controls
            className="w-auto max-h-[32rem] max-w-full self-start rounded-md border"
          />
        ) : (
          <Button asChild variant="outline" size="sm" className="self-start">
            <a href={src} download>
              <DownloadIcon /> Download file
            </a>
          </Button>
        )}

        {/* Caption is untrusted text — rendered as auto-escaped JSX. */}
        {marker.caption && <p className="text-sm">{marker.caption}</p>}
      </CardContent>
    </Card>
  );
}

function Note({ text }: { text: string }) {
  if (!text) return null;
  return (
    <div className="flex items-center justify-center gap-2 py-1 text-center text-xs text-muted-foreground italic">
      <InfoIcon className="size-3.5 shrink-0" />
      <span>{text}</span>
    </div>
  );
}
