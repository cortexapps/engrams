import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { CircleHelp, FilePenLine } from "lucide-react";
import { useEffect, useState } from "react";

import type { SpecListItem } from "../../gen/engram/app/v1/spec_pb";
import { useNow } from "../../hooks/useNow";
import { useSpecs, type SpecLifecycleFilter } from "../../hooks/useSpecs";
import { errorMessage } from "../../lib/errors";
import { relativeTime } from "../sessions/session-format";
import { SpecTicketSyncBadge } from "./SpecTicketSyncBadge";
import { Avatar, AvatarFallback, AvatarGroup, AvatarGroupCount } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";

type StatusSearch = { status?: "draft" | "published" };
const PAGE_SIZE = 50;

export function SpecsList() {
  const search = useSearch({ from: "/_app/specs/" }) as StatusSearch;
  const navigate = useNavigate();
  const lifecycle: SpecLifecycleFilter = search.status ?? "all";
  const [page, setPage] = useState(1);
  const { data, error, isPending } = useSpecs(lifecycle, page, PAGE_SIZE);
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
          value={lifecycle}
          onValueChange={(value) => {
            const status = value as SpecLifecycleFilter;
            selectSpecFilter(status, setPage, (nextSearch) => {
              navigate({ to: "/specs", search: nextSearch });
            });
          }}
        >
          <TabsList aria-label="Filter tech specs">
            <TabsTrigger value="all">All</TabsTrigger>
            <TabsTrigger value="draft">Drafts</TabsTrigger>
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
        <div role="alert" className="rounded-lg border border-dashed py-12 text-center">
          <p className="text-sm text-destructive">
            Couldn’t load tech specs. {errorMessage(error)}
          </p>
        </div>
      ) : data.totalCount === 0 ? (
        <div className="flex flex-col items-center gap-3 rounded-lg border border-dashed py-16 text-center">
          <FilePenLine className="size-10 text-muted-foreground" strokeWidth={1} aria-hidden />
          <p className="max-w-sm text-sm text-muted-foreground">
            {lifecycle === "all" ? "No tech specs yet." : `No ${lifecycle} tech specs.`}
          </p>
        </div>
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
  status: SpecLifecycleFilter,
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
        <Badge variant={spec.lifecycle === "published" ? "outline" : "secondary"}>
          {spec.lifecycle === "published" ? "Published" : "Draft"}
        </Badge>
      </TableCell>
      <TableCell>
        <SpecPeople participants={spec.participants} />
      </TableCell>
      <TableCell>
        <span className="inline-flex items-center gap-1.5 font-mono text-xs tabular-nums">
          <CircleHelp className="size-3.5 text-muted-foreground" aria-hidden />
          {spec.openQuestionCount}
        </span>
      </TableCell>
      <TableCell>
        <SpecTicketSyncBadge state={spec.ticketSyncState} />
      </TableCell>
      <TableCell className="text-right font-mono text-xs tabular-nums text-muted-foreground">
        {relativeTime(spec.updatedAt, now)}
      </TableCell>
    </TableRow>
  );
}

export function SpecPeople({ participants }: { participants: SpecListItem["participants"] }) {
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
      {participants.length > people.length && (
        <AvatarGroupCount className="size-6 text-xs">
          +{participants.length - people.length}
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
    <div role="status" aria-label="Loading tech specs" className="space-y-2">
      {Array.from({ length: 5 }).map((_, index) => (
        <div key={index} className="flex items-center gap-4 border-b px-2 py-3">
          <div className="min-w-0 flex-1 space-y-2">
            <Skeleton className="h-4 w-2/5" />
            <Skeleton className="h-3 w-1/4" />
          </div>
          <Skeleton className="h-5 w-16 rounded-full" />
          <Skeleton className="h-6 w-20" />
          <Skeleton className="h-4 w-10" />
          <Skeleton className="h-4 w-16" />
        </div>
      ))}
    </div>
  );
}
