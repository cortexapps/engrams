import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { Markdown } from "@/components/Markdown";
import { SpecTicketSyncPanel } from "@/components/spec/SpecTicketSyncPanel";
import { SpecTicketTree } from "@/components/spec/SpecTicketTree";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Text } from "@/components/ui/text";
import {
  useSpecDecisions,
  type SpecDecision,
  type SpecDecisionActor,
} from "@/hooks/useSpecDecisions";
import type { SpecPublishQuestion } from "@/hooks/useSpecPublish";
import type { SpecCheckpoint } from "@/hooks/useSpecRead";
import { useSpecTicketCommand, useSpecTickets, writeTree } from "@/hooks/useSpecTickets";
import "./spec-mode.css";

type TicketSurface = "tree" | "sync" | null;

export function SpecPublishedView({
  specId,
  title,
  checkpoint,
  owner,
  currentRevision,
  publishedAt,
  openQuestions,
  onOpenDraft,
}: {
  specId: string;
  title: string;
  checkpoint: SpecCheckpoint;
  owner: SpecDecisionActor | null;
  currentRevision: string;
  publishedAt: string | null;
  openQuestions: SpecPublishQuestion[];
  onOpenDraft: () => void;
}) {
  const decisions = useSpecDecisions(specId);
  const [ticketSurface, setTicketSurface] = useState<TicketSurface>(null);
  const decisionRows = decisions.data ?? [];
  const people = decisionActors(owner, decisionRows);

  if (ticketSurface === "sync") {
    return (
      <div className="spec-mode-published-surface">
        <main className="spec-mode-published-tickets">
          <SpecTicketSyncPanel specId={specId} onBack={() => setTicketSurface(null)} />
        </main>
      </div>
    );
  }
  if (ticketSurface === "tree") {
    return (
      <PublishedTicketTree specId={specId} title={title} onBack={() => setTicketSurface(null)} />
    );
  }

  return (
    // The surface paints its own paper. Without it, this main sat directly on
    // the app shell's dark ground while keeping light-theme ink — the
    // published page rendered dark-on-dark, unreadable.
    <div className="spec-mode-published-surface">
      <main className="spec-mode-published" aria-label="Published spec">
        <header className="spec-mode-published-header">
          <div className="spec-mode-published-meta">
            <Text as="span" variant="label" tone="muted">
              Published
            </Text>
            <Text as="span" variant="code" tone="muted">
              {/* The pinned date, never the internal doc_seq: a first publish
                once introduced itself as "v11". */}
              Published {formatDate(publishedAt ?? checkpoint.createdAt)} · immutable
            </Text>
          </div>
          <Text as="h1" variant="heading" className="spec-mode-published-title">
            {title}
          </Text>
          <div className="spec-mode-published-byline">
            {decisions.isPending ? (
              <Text tone="muted">Loading decision attribution…</Text>
            ) : people.length > 0 ? (
              <Text>Decided by {people.map((person) => person.name).join(", ")} · with engram</Text>
            ) : (
              <Text tone="muted">Decision attribution is not available.</Text>
            )}
            <span aria-hidden="true" />
            <Text as="span" variant="code" tone="muted">
              {checkpoint.sections.length}{" "}
              {checkpoint.sections.length === 1 ? "section" : "sections"}
              {" · "}
              {openQuestions.length} open {openQuestions.length === 1 ? "question" : "questions"}
            </Text>
          </div>
        </header>

        <DecisionCard
          decisions={decisionRows}
          sections={checkpoint.sections}
          pending={decisions.isPending}
          error={decisions.error}
        />

        <PublishedDocument markdown={checkpoint.markdown} decisions={decisionRows} />

        {openQuestions.length > 0 ? (
          <section className="spec-mode-published-questions" aria-labelledby="carried-questions">
            <div>
              <Text as="h2" variant="heading" id="carried-questions">
                Questions carried forward
              </Text>
              <Text tone="muted">
                Publishing acknowledged these questions. It did not resolve them.
              </Text>
            </div>
            <ul>
              {openQuestions.map((question) => (
                <li key={question.id}>
                  <span aria-hidden="true">⚑</span>
                  <span>
                    <Text>{question.text}</Text>
                    <Text as="span" variant="code" tone="muted">
                      Open question · §{question.sectionTitle} · carried into the rollout tickets
                    </Text>
                  </span>
                </li>
              ))}
            </ul>
          </section>
        ) : null}

        <footer className="spec-mode-published-footer">
          <div className="spec-mode-published-footer-actions">
            <Button type="button" variant="outline" onClick={onOpenDraft}>
              Open the draft
            </Button>
            <Button type="button" variant="outline" onClick={() => setTicketSurface("tree")}>
              Open tickets
            </Button>
            <Button type="button" variant="ghost" onClick={() => setTicketSurface("sync")}>
              Sync tickets
            </Button>
          </div>
          {currentRevision !== checkpoint.docSeq ? (
            <Text as="span" variant="code" tone="muted">
              The draft has moved on since this version.
            </Text>
          ) : null}
        </footer>
      </main>
    </div>
  );
}

function DecisionCard({
  decisions,
  sections,
  pending,
  error,
}: {
  decisions: SpecDecision[];
  sections: Array<{ id: string; title: string }>;
  pending: boolean;
  error: Error | null;
}) {
  // The pinned document owns the headings. The server falls back to the raw
  // section id when no transcript action named the section, which rendered
  // "§9d23bd84-…" in the ledger.
  const titles = new Map(sections.map((section) => [section.id, section.title]));
  const sectionTitle = (decision: SpecDecision) =>
    titles.get(decision.sectionId) ?? decision.sectionTitle;
  return (
    <section className="spec-mode-decisions" aria-labelledby="published-decisions">
      <header>
        <Text as="h2" variant="label" id="published-decisions">
          Decisions
        </Text>
        <Text as="span" variant="code" tone="muted">
          {decisions.length} recorded
        </Text>
      </header>
      {pending ? (
        <div className="spec-mode-decisions-loading" aria-label="Loading decisions">
          <Skeleton className="h-4 w-4/5" />
          <Skeleton className="h-4 w-3/5" />
        </div>
      ) : error ? (
        <Text className="spec-mode-decisions-message" tone="destructive" role="alert">
          The decision record is not available. {error.message}
        </Text>
      ) : decisions.length === 0 ? (
        <Text className="spec-mode-decisions-message" tone="muted">
          No section settlements or question resolutions were recorded for this version.
        </Text>
      ) : (
        <ol>
          {decisions.map((decision) => (
            <li key={`${decision.kind}-${decision.id}`}>
              <time dateTime={decision.decidedAt}>{formatTime(decision.decidedAt)}</time>
              <div>
                <Text className="spec-mode-decision-copy">
                  {decision.kind === "section_settled"
                    ? `Settled §${sectionTitle(decision)}`
                    : `Resolved: “${decision.question}”`}
                </Text>
                <Text as="span" variant="code" tone="muted">
                  {decision.actor.name}
                  {decision.kind === "question_resolved" ? ` · §${sectionTitle(decision)}` : ""}
                </Text>
              </div>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}

function PublishedDocument({
  markdown,
  decisions,
}: {
  markdown: string;
  decisions: SpecDecision[];
}) {
  const settledTitles = new Set(
    decisions
      .filter((decision) => decision.kind === "section_settled")
      .map((decision) => normalizeTitle(decision.sectionTitle)),
  );
  return (
    <article className="spec-mode-published-document">
      {markdownSections(markdown).map((section, index) => {
        const sources =
          section.title && settledTitles.has(normalizeTitle(section.title))
            ? provenanceSources(section.markdown)
            : [];
        return (
          <section key={`${section.title ?? "preamble"}-${index}`}>
            <Markdown text={section.markdown} highlightCode />
            {sources.length > 0 ? (
              <div
                className="spec-mode-provenance-chips"
                aria-label={`Sources for ${section.title}`}
              >
                {sources.map((source) => (
                  <Text as="span" variant="code" tone="muted" key={source}>
                    {source}
                  </Text>
                ))}
              </div>
            ) : null}
          </section>
        );
      })}
    </article>
  );
}

function PublishedTicketTree({
  specId,
  title,
  onBack,
}: {
  specId: string;
  title: string;
  onBack: () => void;
}) {
  const tickets = useSpecTickets(specId);
  const command = useSpecTicketCommand(specId);
  const queryClient = useQueryClient();
  return (
    <div className="spec-mode-published-surface">
      <main className="spec-mode-published-tickets">
        <header>
          <div>
            <Text as="span" variant="label" tone="muted">
              Published tickets
            </Text>
            <Text as="h1" variant="heading">
              {title}
            </Text>
          </div>
          <Button type="button" variant="outline" onClick={onBack}>
            Back to spec
          </Button>
        </header>
        {tickets.isPending ? <Skeleton className="h-40 w-full" /> : null}
        {tickets.error ? (
          <Text tone="destructive" role="alert">
            The ticket tree is not available. {tickets.error.message}
          </Text>
        ) : null}
        {tickets.data ? (
          <SpecTicketTree
            tree={tickets.data}
            onCommand={(ticketCommand) => command.mutateAsync(ticketCommand)}
            onTree={(tree) => writeTree(queryClient, specId, tree)}
          />
        ) : null}
      </main>
    </div>
  );
}

export function decisionActors(
  owner: SpecDecisionActor | null,
  decisions: SpecDecision[],
): SpecDecisionActor[] {
  const actors = owner
    ? [owner, ...decisions.map((decision) => decision.actor)]
    : decisions.map((decision) => decision.actor);
  const seen = new Set<string>();
  return actors.filter((actor) => {
    // An unknown actor is not a person, so it cannot appear in the byline —
    // an earlier build credited a deleted-user label as a decision-maker. Test
    // the id, not the label: a missing id is exactly what makes an actor
    // unnameable, and matching the server's wording here would silently stop
    // working the moment that wording changed. A checkpoint author always
    // carries an id, so a real owner is never dropped.
    if (actor.id === null) return false;
    const key = actor.id ?? `name:${actor.name}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

interface MarkdownSection {
  title: string | null;
  markdown: string;
}

export function markdownSections(markdown: string): MarkdownSection[] {
  const headings = [...markdown.matchAll(/^##[ \t]+(.+?)[ \t]*$/gm)];
  if (headings.length === 0) return [{ title: null, markdown }];
  const sections: MarkdownSection[] = [];
  const firstIndex = headings[0]?.index ?? 0;
  if (firstIndex > 0 && markdown.slice(0, firstIndex).trim().length > 0) {
    sections.push({ title: null, markdown: markdown.slice(0, firstIndex) });
  }
  headings.forEach((heading, index) => {
    const start = heading.index ?? 0;
    const end = headings[index + 1]?.index ?? markdown.length;
    sections.push({ title: heading[1]?.trim() ?? null, markdown: markdown.slice(start, end) });
  });
  return sections;
}

const PROVENANCE = /[A-Za-z0-9_./-]+(?::[0-9-]+)?\s+@\s+[0-9a-f]{7,40}/gi;

export function provenanceSources(markdown: string): string[] {
  return [...new Set(markdown.match(PROVENANCE) ?? [])];
}

function normalizeTitle(title: string): string {
  return title.trim().toLocaleLowerCase();
}

function formatDate(value: string): string {
  return new Intl.DateTimeFormat(undefined, { day: "numeric", month: "short" }).format(
    new Date(value),
  );
}

function formatTime(value: string): string {
  return new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" }).format(
    new Date(value),
  );
}
