import { Link, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { FileBox } from "lucide-react";

import { ArtifactViewer } from "../../components/artifacts/ArtifactViewer";
import { useTheme } from "../../components/theme-provider";
import { useArtifact } from "../../hooks/useArtifacts";
import { artifactBytesUrl, mediaKind } from "../../lib/artifacts";
import { errorMessage } from "../../lib/errors";
import { EngramMark } from "../../components/EngramMark";
import { Skeleton } from "@/components/ui/skeleton";
import { Text } from "@/components/ui/text";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

// The standalone artifact view — what the detail page's popout opens.
// A chromeless full-window page: one slim engrams-toned bar (mark,
// title, revision picker) over the RENDERED document — styled markdown
// in the reading column, HTML live in its sandboxed frame — never the
// raw bytes. Sits outside the app shell so the document owns the
// window; the mark links back to the full detail page.
export function ArtifactViewPage() {
  const { artifactId } = useParams({ strict: false }) as { artifactId: string };
  const search = useSearch({ strict: false }) as { v?: number };
  const navigate = useNavigate();
  const { theme } = useTheme();
  const { data, isPending, error } = useArtifact(artifactId);
  const artifact = data?.artifact;

  if (isPending) {
    return (
      <div className="flex h-svh flex-col">
        <div className="flex h-12 items-center border-b px-4">
          <Skeleton className="h-4 w-48" />
        </div>
        <div className="flex-1 p-6">
          <Skeleton className="h-64 w-full" />
        </div>
      </div>
    );
  }
  if (error || !artifact) {
    return (
      <div className="flex h-svh flex-col items-center justify-center gap-3 text-muted-foreground">
        <FileBox className="size-10" strokeWidth={1} />
        <Text as="p" tone="muted">
          {error ? errorMessage(error) : "Artifact not found."}
        </Text>
      </div>
    );
  }

  const version =
    search.v !== undefined && artifact.versions.some((v) => v.version === search.v)
      ? search.v
      : artifact.currentVersion;
  const isCurrent = version === artifact.currentVersion;
  const versionRow = artifact.versions.find((v) => v.version === version);
  const kind = mediaKind(versionRow?.mediaType ?? artifact.mediaType);
  const cookieUrl = artifactBytesUrl(artifact.id, isCurrent ? {} : { v: version });
  const htmlSrc = `${artifact.rawUrl}${isCurrent ? "" : `&v=${version}`}&theme=${theme}`;

  return (
    <div className="flex h-svh flex-col bg-background text-foreground">
      <header className="flex h-12 shrink-0 items-center gap-3 border-b px-4">
        <Link
          to="/artifacts/$artifactId"
          params={{ artifactId: artifact.id }}
          className="flex items-center text-primary"
          aria-label="Open in engrams"
        >
          <EngramMark size={20} mode="static" />
        </Link>
        <Text as="span" variant="label" tone="muted">
          artifact
        </Text>
        <span className="min-w-0 truncate text-sm font-medium">{artifact.title}</span>
        <div className="ml-auto flex items-center gap-2">
          {artifact.versions.length > 1 && (
            <Select
              value={String(version)}
              onValueChange={(value) => {
                const v = Number(value);
                navigate({
                  to: "/artifacts/$artifactId/view",
                  params: { artifactId: artifact.id },
                  search: v === artifact.currentVersion ? {} : { v },
                  replace: true,
                });
              }}
            >
              <SelectTrigger size="sm" aria-label="Revision">
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
          {!isCurrent && (
            <Text as="span" variant="label" tone="muted">
              read-only
            </Text>
          )}
        </div>
      </header>

      <div
        className={
          kind === "html"
            ? "flex min-h-0 flex-1 flex-col"
            : "min-h-0 flex-1 overflow-y-auto px-4 pb-10 md:px-6"
        }
      >
        <ArtifactViewer
          key={`${version}-${theme}`}
          url={cookieUrl}
          {...(kind === "html" ? { htmlSrc } : {})}
          mediaType={versionRow?.mediaType ?? artifact.mediaType}
          fileName={artifact.fileName}
          variant="full"
        />
      </div>
    </div>
  );
}
