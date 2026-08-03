import { Link } from "@tanstack/react-router";
import { Globe } from "lucide-react";

import type { ArtifactRecord } from "../../gen/engram/app/v1/artifact_pb";
import { useNow } from "../../hooks/useNow";
import { artifactBytesUrl, extensionOf, KIND_GLYPHS, mediaKind } from "../../lib/artifacts";
import { relativeTime } from "../sessions/session-format";
import { fmtBytes } from "../../components/transcriptFmt";
import { Text } from "@/components/ui/text";

// One gallery tile. Images use their own bytes as the cover; every other
// kind gets a "spec plate": muted ground, hairline border, the kind glyph
// with the extension in the instrument-label voice.
export function ArtifactCard({
  artifact,
  showOwner,
}: {
  artifact: ArtifactRecord;
  showOwner: boolean;
}) {
  const now = useNow();
  const kind = mediaKind(artifact.mediaType);
  const Glyph = KIND_GLYPHS[kind];
  const ext = extensionOf(artifact.fileName);

  return (
    <Link
      to="/artifacts/$artifactId"
      params={{ artifactId: artifact.id }}
      className="group flex flex-col overflow-hidden rounded-md border transition-colors hover:border-ring"
    >
      <div className="flex h-36 items-center justify-center border-b bg-muted/40">
        {kind === "image" ? (
          <img
            src={artifactBytesUrl(artifact.id)}
            alt={artifact.title}
            loading="lazy"
            className="size-full object-cover"
          />
        ) : (
          <div className="flex flex-col items-center gap-2 text-muted-foreground">
            <Glyph className="size-8" strokeWidth={1.25} />
            {ext && (
              <Text as="span" variant="label" tone="muted">
                {ext}
              </Text>
            )}
          </div>
        )}
      </div>
      <div className="flex flex-col gap-1 p-3">
        <div className="flex items-center gap-1.5">
          <span className="min-w-0 flex-1 truncate text-sm font-medium">{artifact.title}</span>
          {artifact.visibility === "org" && (
            <Globe
              className="size-3.5 shrink-0 text-muted-foreground"
              aria-label="Shared with the org"
            />
          )}
        </div>
        <div className="flex items-center gap-1.5 font-mono text-[0.7rem] text-muted-foreground">
          <span className="truncate">{artifact.mediaType}</span>
          <span aria-hidden>·</span>
          <span className="tabular-nums">{fmtBytes(Number(artifact.sizeBytes))}</span>
          <span aria-hidden>·</span>
          <span className="tabular-nums">v{artifact.currentVersion}</span>
          <span className="ml-auto shrink-0 tabular-nums">
            {relativeTime(artifact.updatedAt, now)}
          </span>
        </div>
        {showOwner && artifact.createdBy && (
          <Text as="span" variant="body" tone="muted" className="truncate text-xs">
            {artifact.createdBy.name || artifact.createdBy.email}
          </Text>
        )}
      </div>
    </Link>
  );
}
