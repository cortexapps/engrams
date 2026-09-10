import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { CircleHelp } from "lucide-react";
import { useEffect, useState } from "react";

import type { SpecListItem } from "../../gen/engram/app/v1/spec_pb";
import { useNow } from "../../hooks/useNow";
import { useSpecs, type SpecPhaseFilter } from "../../hooks/useSpecs";
import { errorMessage } from "../../lib/errors";
import { relativeAge } from "@/lib/relative-time";
import { SpecTicketSyncBadge } from "./SpecTicketSyncBadge";
import { Avatar, AvatarFallback, AvatarGroup, AvatarGroupCount } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { StatusDot } from "@/components/status-dot";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";

type StatusSearch = { status?: "ideation" | "drafting" | "published" };
const PAGE_SIZE = 50;

export function SpecsList() {
  const search = useSearch({ from: "/_app/specs/" }) as StatusSearch;
  const navigate = useNavigate();
  const phase: SpecPhaseFilter = search.status ?? "all";
  const [page, setPage] = useState(1);
  const { data, error, isPending } = useSpecs(phase, page, PAGE_SIZE);
  const now = useNow();
  const totalCount = data?.totalCount;

  useEffect(() => {
    if (totalCount === undefined) return;
    const lastPage = Math.max(1, Math.ceil(totalCount / PAGE_SIZE));
    if (page > lastPage) setPage(lastPage);
  }, [page, totalCount]);

  return (
    <div className="min-h-0 flex-1 overflow-auto p-4 md:p-6">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
        <Tabs
          value={phase}
          onValueChange={(value) => {
            const status = value as SpecPhaseFilter;
            selectSpecFilter(status, setPage, (nextSearch) => {
              navigate({ to: "/specs", search: nextSearch });
            });
          }}
        >
          <TabsList aria-label="Filter tech specs">
            <TabsTrigger value="all">All</TabsTrigger>
            <TabsTrigger value="ideation">Ideation</TabsTrigger>
            <TabsTrigger value="drafting">Drafting</TabsTrigger>
            <TabsTrigger value="published">Published</TabsTrigger>
          </TabsList>
        </Tabs>
        {data && (
          <span className="font-mono text-xs tabular-nums text-muted-foreground">
            {data.totalCount} {data.totalCount === 1 ? "spec" : "specs"}
          </span>
        )}
      </div>

      {isPending ? (
        <SpecListSkeleton />
      ) : error ? (
        <EmptyState tone="error">Couldn’t load tech specs. {errorMessage(error)}</EmptyState>
      ) : data.totalCount === 0 ? (
        <EmptyState>
          {phase === "all" ? "No tech specs yet." : `No ${phase} tech specs.`}
        </EmptyState>
      ) : (
        <>
          <Table className="min-w-[880px]">
            <TableHeader>
              <TableRow>
                <TableHead className="w-full">Title</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>People</TableHead>
                <TableHead>Questions</TableHead>
                <TableHead>Tickets</TableHead>
                <TableHead className="text-right">Updated</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {data.specs.map((spec) => (
                <SpecRow key={spec.id} spec={spec} now={now} />
              ))}
            </TableBody>
          </Table>
          <SpecPagination
            page={page}
            pageSize={PAGE_SIZE}
            totalCount={data.totalCount}
            onPageChange={setPage}
          />
        </>
      )}
    </div>
  );
}

export function selectSpecFilter(
  status: SpecPhaseFilter,
  setPage: (page: number) => void,
  navigate: (search: StatusSearch) => void,
): void {
  setPage(1);
  navigate(status === "all" ? {} : { status });
}

export function SpecPagination({
  page,
  pageSize,
  totalCount,
  onPageChange,
}: {
  page: number;
  pageSize: number;
  totalCount: number;
  onPageChange: (page: number) => void;
}) {
  const totalPages = Math.max(1, Math.ceil(totalCount / pageSize));
  if (totalPages === 1 && page === 1) return null;
  return (
    <div className="mt-4 flex items-center justify-end gap-3">
      <Button variant="outline" disabled={page === 1} onClick={() => onPageChange(page - 1)}>
        Previous
      </Button>
      <span className="font-mono text-xs text-muted-foreground">
        Page {page} of {totalPages}
      </span>
      <Button
        variant="outline"
        disabled={page >= totalPages}
        onClick={() => onPageChange(page + 1)}
      >
        Next
      </Button>
    </div>
  );
}

export function SpecRow({ spec, now }: { spec: SpecListItem; now: number }) {
  return (
    <TableRow>
      <TableCell className="min-w-64 whitespace-normal py-3">
        <Link
          to="/specs/$specId"
          params={{ specId: spec.id }}
          className="font-medium hover:underline"
        >
          {spec.title}
        </Link>
        <p className="mt-0.5 truncate text-xs text-muted-foreground">
          {[spec.templateName, spec.repo].filter(Boolean).join(" · ")}
        </p>
      </TableCell>
      <TableCell>
        <Badge variant={spec.phase === "published" ? "outline" : "secondary"}>
          <StatusDot
            size={6}
            tone={
              spec.phase === "published"
                ? "nominal"
                : spec.phase === "drafting"
                  ? "active"
                  : "caution"
            }
          />
          {spec.phase === "published"
            ? "Published"
            : spec.phase === "ideation"
              ? "Ideation"
              : "Drafting"}
        </Badge>
      </TableCell>
      <TableCell>
        <SpecPeople
          participants={spec.participants}
          activeParticipantCount={spec.activeParticipantCount}
        />
      </TableCell>
      <TableCell>
        {spec.openQuestionCount > 0 ? (
          <span className="inline-flex items-center gap-1.5 font-mono text-xs tabular-nums">
            <CircleHelp className="size-3.5 text-muted-foreground" aria-hidden />
            {spec.openQuestionCount}
          </span>
        ) : (
          <span aria-label="No open questions" className="text-muted-foreground">
            —
          </span>
        )}
      </TableCell>
      <TableCell>
        {/* The badge is deliberately absent before a ticket draft exists, but
            the cell still needs the same em dash People and Questions use —
            a blank cell on every row reads as a column that failed to load. */}
        {spec.ticketSyncState && spec.ticketSyncState !== "none" ? (
          <SpecTicketSyncBadge state={spec.ticketSyncState} />
        ) : (
          <span aria-label="No tickets" className="text-muted-foreground">
            —
          </span>
        )}
      </TableCell>
      <TableCell className="text-right font-mono text-xs tabular-nums text-muted-foreground">
        {relativeAge(spec.updatedAt, now)}
      </TableCell>
    </TableRow>
  );
}

export function SpecPeople({
  participants,
  activeParticipantCount,
}: {
  participants: SpecListItem["participants"];
  activeParticipantCount: number;
}) {
  const people = participants.slice(0, 3);
  if (people.length === 0) {
    return (
      <span aria-label="No live collaborators" className="text-muted-foreground">
        —
      </span>
    );
  }

  return (
    <AvatarGroup aria-label="Live collaborators">
      {people.map((person) => (
        <Avatar
          key={person.id}
          size="sm"
          aria-label={person.name || person.email}
          title={person.name || person.email}
        >
          <AvatarFallback>{initials(person.name || person.email)}</AvatarFallback>
        </Avatar>
      ))}
      {activeParticipantCount > people.length && (
        <AvatarGroupCount className="size-6 text-xs">
          +{activeParticipantCount - people.length}
        </AvatarGroupCount>
      )}
    </AvatarGroup>
  );
}

function initials(value: string): string {
  const parts = value.trim().split(/\s+/).filter(Boolean);
  return (
    parts
      .slice(0, 2)
      .map((part) => part[0]?.toUpperCase())
      .join("") || "?"
  );
}

function SpecListSkeleton() {
  return (
    <SkeletonRows
      rows={5}
      columns={["minmax(0,1fr)", "72px", "80px", "64px", "64px", "56px"]}
      className="min-w-[880px]"
    />
  );
}
