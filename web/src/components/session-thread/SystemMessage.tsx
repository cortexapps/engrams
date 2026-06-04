import { useAuiState } from '@assistant-ui/react';
import {
  CameraIcon,
  DownloadIcon,
  ExternalLinkIcon,
  GitPullRequestIcon,
  InfoIcon,
  RotateCcwIcon,
} from 'lucide-react';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { API_BASE } from '../../api';
import { fmtBytes, hms } from '../transcriptFmt';
import type { SystemMarker } from './buildMessages';

// The "harness register": non-message timeline events (durability markers,
// opened PRs, shared artifacts, system notes) carried as system messages.
// Visually distinct from the agent's tool calls — durability/notes are faint
// centered markers; PRs and artifacts are framed cards. Switches on the
// marker payload stashed in metadata.custom by buildMessages.

export function SystemMessage() {
  const marker = useAuiState(
    (s) => s.message.metadata.custom?.marker as SystemMarker | undefined,
  );
  const fallback = useAuiState((s) => {
    const part = s.message.content[0];
    return part?.type === 'text' ? part.text : '';
  });

  if (!marker) return <Note text={fallback} />;

  switch (marker.kind) {
    case 'durability':
      return <Durability marker={marker} />;
    case 'pull_request':
      return <PullRequest marker={marker} />;
    case 'artifact':
      return <Artifact marker={marker} />;
    case 'note':
      return <Note text={fallback} />;
  }
}

function Durability({
  marker,
}: {
  marker: Extract<SystemMarker, { kind: 'durability' }>;
}) {
  const Icon = marker.mark === 'snapshot' ? CameraIcon : RotateCcwIcon;
  const label = marker.mark === 'snapshot' ? 'snapshotted' : 'resumed';
  return (
    <div className="flex items-center justify-center gap-2 py-1 text-xs text-muted-foreground">
      <Icon className="size-3.5" />
      <span className="uppercase tracking-wide">{label}</span>
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

function PullRequest({
  marker,
}: {
  marker: Extract<SystemMarker, { kind: 'pull_request' }>;
}) {
  return (
    <Card className="py-0">
      <CardContent className="flex flex-col gap-1.5 p-4">
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <GitPullRequestIcon className="size-3.5 text-primary" />
          <span className="uppercase tracking-wide">pull request</span>
          <span aria-hidden>·</span>
          <span className="font-mono">
            {marker.repo} #{marker.number}
          </span>
          <span className="ml-auto font-mono tabular-nums">{hms(marker.at)}</span>
        </div>
        <a
          href={marker.url}
          target="_blank"
          rel="noreferrer"
          className="group inline-flex items-baseline gap-1 text-base font-medium hover:underline"
        >
          {marker.title}
          <ExternalLinkIcon className="size-3.5 shrink-0 self-center text-muted-foreground" />
        </a>
        <div className="flex items-center gap-1.5 font-mono text-xs text-muted-foreground">
          <Badge variant="outline" className="font-normal">
            {marker.headBranch}
          </Badge>
          <span aria-hidden>→</span>
          <Badge variant="outline" className="font-normal">
            {marker.baseBranch}
          </Badge>
        </div>
      </CardContent>
    </Card>
  );
}

function Artifact({
  marker,
}: {
  marker: Extract<SystemMarker, { kind: 'artifact' }>;
}) {
  // Same-origin GET; the browser carries the auth cookie / dev proxy. No
  // bearer needed for a passive <img>/<video>.
  const src = `${API_BASE}/sessions/${marker.sessionId}/artifacts/${marker.artifactId}`;
  const isImage = marker.mediaType.startsWith('image/');
  const isVideo = marker.mediaType.startsWith('video/');

  return (
    <Card className="overflow-hidden py-0">
      <CardContent className="flex flex-col gap-2 p-4">
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <DownloadIcon className="size-3.5 text-primary" />
          <span className="uppercase tracking-wide">shared file</span>
          <span aria-hidden>·</span>
          <span className="font-mono">{marker.mediaType}</span>
          <span aria-hidden>·</span>
          <span className="tabular-nums">{fmtBytes(marker.sizeBytes)}</span>
          <span className="ml-auto font-mono tabular-nums">{hms(marker.at)}</span>
        </div>

        {isImage ? (
          <img
            src={src}
            alt={marker.caption ?? 'shared image'}
            className="max-h-[32rem] max-w-full rounded-md border"
          />
        ) : isVideo ? (
          // eslint-disable-next-line jsx-a11y/media-has-caption
          <video
            src={src}
            controls
            className="max-h-[32rem] max-w-full rounded-md border"
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
