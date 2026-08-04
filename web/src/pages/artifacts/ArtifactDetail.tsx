import { Link, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { Check, Copy, DownloadIcon, ExternalLinkIcon, FileBox } from "lucide-react";
import { useState } from "react";

import { useTheme } from "../../components/theme-provider";
import { ArtifactViewer } from "../../components/artifacts/ArtifactViewer";
import { GuardedDownload } from "../../components/artifacts/GuardedDownload";
import { ShareDialog } from "../../components/artifacts/ShareDialog";
import { fmtBytes } from "../../components/transcriptFmt";
import type { ArtifactRecord } from "../../gen/engram/app/v1/artifact_pb";
import { subject } from "@casl/ability";

import { useArtifact } from "../../hooks/useArtifacts";
import { useAuth } from "../../auth/AuthProvider";
import { artifactBytesUrl, artifactPageUrl, mediaKind } from "../../lib/artifacts";
import { errorMessage } from "../../lib/errors";
import { PageHeading } from "@/components/page-heading";
import { Button, buttonVariants } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Text } from "@/components/ui/text";

// The artifact page: a themed masthead + toolbar around the document
// itself. HTML fills the remaining height inside its sandboxed frame;
// markdown gets the reading column; everything else centers.
export function ArtifactDetail() {
  const { artifactId } = useParams({ strict: false }) as { artifactId: string };
  const search = useSearch({ strict: false }) as { v?: number };
  const { data, isPending, error } = useArtifact(artifactId);
  const artifact = data?.artifact;

  if (isPending) {
    return (
      <div className="flex-1 space-y-4 p-4 md:p-6">
        <Skeleton className="h-16 w-full max-w-xl" />
        <Skeleton className="h-64 w-full" />
      </div>
    );
  }
  if (error || !artifact) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-3 text-muted-foreground">
        <FileBox className="size-10" strokeWidth={1} />
        <Text as="p" tone="muted">
          {error ? errorMessage(error) : "Artifact not found."}
        </Text>
        <Button asChild variant="outline" size="sm">
          <Link to="/artifacts">Back to artifacts</Link>
        </Button>
      </div>
    );
  }
  return <ArtifactBody artifact={artifact} requestedVersion={search.v} />;
}

function ArtifactBody({
  artifact,
  requestedVersion,
}: {
  artifact: ArtifactRecord;
  requestedVersion?: number;
}) {
  const { theme } = useTheme();
  const version =
    requestedVersion !== undefined && artifact.versions.some((v) => v.version === requestedVersion)
      ? requestedVersion
      : artifact.currentVersion;
  const isCurrent = version === artifact.currentVersion;
  const versionRow = artifact.versions.find((v) => v.version === version);
  const kind = mediaKind(versionRow?.mediaType ?? artifact.mediaType);

  // Cookie-authed URL for downloads/fetches; the tokened raw_url (which
  // already carries ?token=) gains v/theme params for the sandboxed
  // iframe, which sends no cookies. Remount the frame on theme change so
  // an artifact honoring ?theme repaints.
  const cookieUrl = artifactBytesUrl(artifact.id, isCurrent ? {} : { v: version });
  const htmlSrc = `${artifact.rawUrl}${isCurrent ? "" : `&v=${version}`}&theme=${theme}`;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="shrink-0 px-4 pt-4 md:px-6">
        <PageHeading
          eyebrow={`artifact · ${kind}`}
          title={artifact.title}
          description={
            <span className="font-mono text-xs">
              {artifact.fileName} · {versionRow?.mediaType ?? artifact.mediaType} ·{" "}
              {fmtBytes(Number(versionRow?.sizeBytes ?? artifact.sizeBytes))}
              {artifact.createdBy && <> · {artifact.createdBy.name || artifact.createdBy.email}</>}
              {versionRow?.sessionId && (
                <>
                  {" · from "}
                  <Link
                    to="/sessions/$id"
                    params={{ id: versionRow.sessionId }}
                    className="underline-offset-2 hover:underline"
                  >
                    session {versionRow.sessionId.slice(0, 8)}
                  </Link>
                </>
              )}
            </span>
          }
          actions={<ArtifactToolbar artifact={artifact} version={version} url={cookieUrl} />}
        />
        {!isCurrent && (
          <div className="mt-2 rounded-md border border-instrument-caution/40 bg-muted/40 px-3 py-1.5">
            <Text as="span" variant="label" tone="muted">
              viewing v{version} — read-only ·{" "}
              <Link to="/artifacts/$artifactId" params={{ artifactId: artifact.id }}>
                jump to v{artifact.currentVersion}
              </Link>
            </Text>
          </div>
        )}
      </div>

      <div
        className={
          kind === "html"
            ? "mt-4 flex min-h-0 flex-1 flex-col border-t"
            : "min-h-0 flex-1 overflow-y-auto px-4 pb-8 md:px-6"
        }
      >
        {kind === "html" ? (
          <ArtifactViewer
            key={`${version}-${theme}`}
            url={cookieUrl}
            htmlSrc={htmlSrc}
            mediaType={versionRow?.mediaType ?? artifact.mediaType}
            fileName={artifact.fileName}
            variant="full"
          />
        ) : (
          <ArtifactViewer
            key={version}
            url={cookieUrl}
            mediaType={versionRow?.mediaType ?? artifact.mediaType}
            fileName={artifact.fileName}
            variant="full"
          />
        )}
      </div>
    </div>
  );
}

function ArtifactToolbar({
  artifact,
  version,
  url,
}: {
  artifact: ArtifactRecord;
  version: number;
  url: string;
}) {
  const navigate = useNavigate();
  // The mirrored CASL rules decide who may share — owner or admin.
  const { ability } = useAuth();
  const canShare = ability.can(
    "share",
    subject("Artifact", {
      ownerUserId: artifact.ownerUserId ?? null,
      visibility: artifact.visibility,
    }),
  );
  const [copied, setCopied] = useState(false);

  const copyLink = () => {
    const link = `${window.location.origin}${artifactPageUrl(artifact.id, artifact.title)}`;
    void navigator.clipboard.writeText(link).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };

  return (
    <div className="flex flex-wrap items-center gap-2">
      {artifact.versions.length > 1 && (
        <Select
          value={String(version)}
          onValueChange={(value) => {
            const v = Number(value);
            navigate({
              to: "/artifacts/$artifactId",
              params: { artifactId: artifact.id },
              search: v === artifact.currentVersion ? {} : { v },
            });
          }}
        >
          <SelectTrigger size="sm" aria-label="Version">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {artifact.versions.map((v) => (
              <SelectItem key={v.version} value={String(v.version)}>
                v{v.version}
                {v.version === artifact.currentVersion ? " · current" : ""}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      )}

      <Button variant="ghost" size="sm" onClick={copyLink} aria-label="Copy link">
        {copied ? <Check /> : <Copy />}
      </Button>
      {/* Popout: the RENDERED standalone view (styled markdown / sandboxed
          HTML with its own revision picker), not the raw bytes. */}
      <Button asChild variant="ghost" size="sm" aria-label="Open in new tab">
        <a
          href={`/artifacts/${encodeURIComponent(artifact.id)}/view${
            version === artifact.currentVersion ? "" : `?v=${version}`
          }`}
          target="_blank"
          rel="noreferrer"
        >
          <ExternalLinkIcon />
        </a>
      </Button>
      <GuardedDownload
        url={url}
        mediaType={artifact.mediaType}
        fileName={artifact.fileName}
        sizeBytes={Number(artifact.sizeBytes)}
        className={buttonVariants({ variant: "ghost", size: "sm" })}
        aria-label="Download"
      >
        <DownloadIcon />
      </GuardedDownload>

      {canShare && <ShareDialog artifact={artifact} />}
    </div>
  );
}
