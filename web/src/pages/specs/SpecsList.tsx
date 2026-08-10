import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { Bot, CircleHelp, FilePenLine } from "lucide-react";

import type { SpecListItem } from "../../gen/engram/app/v1/spec_pb";
import { useNow } from "../../hooks/useNow";
import { useSpecs, type SpecLifecycleFilter } from "../../hooks/useSpecs";
import { errorMessage } from "../../lib/errors";
import { relativeTime } from "../sessions/session-format";
import { SpecTicketSyncBadge } from "./SpecTicketSyncBadge";
import { Avatar, AvatarFallback, AvatarGroup, AvatarGroupCount } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
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

export function SpecsList() {
  const search = useSearch({ from: "/_app/specs/" }) as StatusSearch;
  const navigate = useNavigate();
  const lifecycle: SpecLifecycleFilter = search.status ?? "all";
  const { data, error, isPending } = useSpecs(lifecycle);
  const now = useNow();

  return (
    <div className="min-h-0 flex-1 overflow-auto p-4 md:p-6">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
        <Tabs
          value={lifecycle}
          onValueChange={(value) => {
            const status = value as SpecLifecycleFilter;
            navigate({
              to: "/specs",
              search: status === "all" ? {} : { status },
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
      ) : data.specs.length === 0 ? (
        <div className="flex flex-col items-center gap-3 rounded-lg border border-dashed py-16 text-center">
          <FilePenLine className="size-10 text-muted-foreground" strokeWidth={1} aria-hidden />
          <p className="max-w-sm text-sm text-muted-foreground">
            {lifecycle === "all" ? "No tech specs yet." : `No ${lifecycle} tech specs.`}
          </p>
        </div>
      ) : (
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
      )}
    </div>
  );
}

function SpecRow({ spec, now }: { spec: SpecListItem; now: number }) {
  const people = spec.participants.slice(0, 3);
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
        <AvatarGroup aria-label="Live collaborators">
          <Avatar size="sm" aria-label="Spec agent" title="Spec agent">
            <AvatarFallback className="bg-instrument-nominal text-background">
              <Bot className="size-3.5" aria-hidden />
            </AvatarFallback>
          </Avatar>
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
          {spec.participants.length > people.length && (
            <AvatarGroupCount className="size-6 text-xs">
              +{spec.participants.length - people.length}
            </AvatarGroupCount>
          )}
        </AvatarGroup>
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
