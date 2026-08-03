import { useMemo, useState } from "react";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { FileBox } from "lucide-react";

import { useIsAdmin } from "../../auth/AuthProvider";
import { useArtifacts, type ArtifactScope } from "../../hooks/useArtifacts";
import { errorMessage } from "../../lib/errors";
import { ArtifactCard } from "./ArtifactCard";
import { PageHeading } from "@/components/page-heading";
import { TabRow } from "@/components/TabRow";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { Text } from "@/components/ui/text";

// The gallery index: a card grid of the caller's documents. Scope rides
// the URL (?scope=shared|all) so a filtered view is linkable; non-admins
// asking for `all` coerce to their default (mirrors ListTasks).
type ScopeTab = "mine" | "shared" | "all";

export function ArtifactsLibrary() {
  const isAdmin = useIsAdmin();
  const navigate = useNavigate();
  const search = useSearch({ from: "/_app/artifacts/" }) as { scope?: ScopeTab };
  const requested: ScopeTab = search.scope ?? "mine";
  const scope: ScopeTab = requested === "all" && !isAdmin ? "mine" : requested;
  const { data, isPending, error } = useArtifacts(scope as ArtifactScope);
  const [filter, setFilter] = useState("");

  const rows = useMemo(() => {
    const all = data?.artifacts ?? [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return all;
    return all.filter(
      (a) =>
        a.title.toLowerCase().includes(needle) ||
        a.fileName.toLowerCase().includes(needle) ||
        a.mediaType.toLowerCase().includes(needle),
    );
  }, [data?.artifacts, filter]);

  const tabs: Array<{ id: ScopeTab; label: string }> = [
    { id: "mine", label: "Mine" },
    { id: "shared", label: "Shared" },
    ...(isAdmin ? [{ id: "all" as const, label: "All" }] : []),
  ];

  return (
    <div className="flex-1 overflow-y-auto p-4 md:p-6">
      <PageHeading
        title="Artifacts"
        description="Documents your agents published — versioned, hosted, shareable."
        showRule={false}
      />
      <TabRow
        tabs={tabs}
        active={scope}
        onChange={(id) =>
          navigate({
            to: "/artifacts",
            search: id === "mine" ? {} : { scope: id },
          })
        }
        right={data ? `${data.totalCount} total` : undefined}
      />

      <div className="mb-4 max-w-xs">
        <Input
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Filter by title, file, type…"
          aria-label="Filter artifacts"
        />
      </div>

      {isPending ? (
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-4">
          {Array.from({ length: 6 }).map((_, i) => (
            <Skeleton key={i} className="h-52 rounded-md" />
          ))}
        </div>
      ) : error ? (
        <Text as="p" tone="destructive" className="py-8">
          {errorMessage(error)}
        </Text>
      ) : rows.length === 0 ? (
        <div className="flex flex-col items-center gap-3 py-20 text-muted-foreground">
          <FileBox className="size-10" strokeWidth={1} />
          <Text as="p" variant="body" tone="muted">
            {filter
              ? "No artifacts match the filter."
              : scope === "shared"
                ? "Nothing has been shared with the org yet."
                : "No artifacts yet — agents publish documents here with the Artifact tool."}
          </Text>
        </div>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-4">
          {rows.map((a) => (
            <ArtifactCard key={a.id} artifact={a} showOwner={scope !== "mine"} />
          ))}
        </div>
      )}
    </div>
  );
}
