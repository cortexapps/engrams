import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";

import { renderWithProviders } from "@/test-utils";
import { SpecPublishedView, decisionActors, provenanceSources } from "./SpecPublishedView";

const decisions = [
  {
    id: "settle-data",
    kind: "section_settled" as const,
    sectionId: "data",
    sectionTitle: "Data model",
    actor: { id: "priya", name: "Priya Raman" },
    decidedAt: "2026-08-13T18:00:00.000Z",
  },
  {
    id: "resolve-lock",
    kind: "question_resolved" as const,
    sectionId: "data",
    sectionTitle: "Data model",
    question: "Which lock coordinates writers?",
    resolutionLink: "section:data@resolution",
    actor: { id: "owner", name: "Nikhil Unni" },
    decidedAt: "2026-08-13T18:01:00.000Z",
  },
];

vi.mock("@/hooks/useSpecDecisions", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/hooks/useSpecDecisions")>();
  return {
    ...original,
    useSpecDecisions: () => ({ data: decisions, isPending: false, error: null }),
  };
});

vi.mock("@/hooks/useSpecTickets", () => ({
  useSpecTickets: () => ({ data: undefined, isPending: false, error: null }),
  useSpecTicketCommand: () => ({ mutateAsync: vi.fn() }),
  writeTree: vi.fn(),
}));

vi.mock("@/components/spec/SpecTicketSyncPanel", () => ({
  SpecTicketSyncPanel: ({ onBack }: { onBack: () => void }) => (
    <section aria-label="Ticket sync">
      <button type="button" onClick={onBack}>
        Back
      </button>
    </section>
  ),
}));

function renderView(onOpenDraft = vi.fn()) {
  return renderWithProviders(
    <SpecPublishedView
      specId="spec-1"
      title="Quota design"
      checkpoint={{
        id: "published-18",
        label: "Published",
        authorUserId: "owner",
        reason: "publish",
        docSeq: "18",
        createdAt: "2026-08-13T18:02:00.000Z",
        markdown: [
          "## Data model",
          "",
          "The writer lock lives in `src/locks.ts @ abcdef1`.",
          "",
          "## Failure modes",
          "",
          "The retry path is in `src/retry.ts @ 1234567`.",
        ].join("\n"),
        sections: [
          { id: "data", title: "Data model" },
          { id: "failure", title: "Failure modes" },
        ],
      }}
      owner={{ id: "owner", name: "Nikhil Unni" }}
      currentRevision="20"
      publishedAt="2026-08-13T18:02:00.000Z"
      openQuestions={[
        {
          id: "q-drain",
          sectionId: "failure",
          sectionTitle: "Failure modes",
          text: "How long can draining take?",
        },
      ]}
      onOpenDraft={onOpenDraft}
    />,
  );
}

describe("SpecPublishedView", () => {
  test("makes attribution, decision times, and carried questions the read view", async () => {
    const onOpenDraft = vi.fn();
    const view = renderView(onOpenDraft);

    expect(
      await screen.findByText("Decided by Nikhil Unni, Priya Raman · with engram"),
    ).toBeTruthy();
    expect(screen.getByText("Settled §Data model")).toBeTruthy();
    expect(screen.getByText("Resolved: “Which lock coordinates writers?”")).toBeTruthy();
    expect(view.container.querySelector('time[datetime="2026-08-13T18:00:00.000Z"]')).toBeTruthy();
    expect(
      screen.getByText("Publishing acknowledged these questions. It did not resolve them."),
    ).toBeTruthy();
    expect(screen.getByText("How long can draining take?")).toBeTruthy();
    expect(screen.getByText("The draft has moved on since this version.")).toBeTruthy();

    const chips = view.container.querySelectorAll(".spec-mode-provenance-chips span");
    expect([...chips].map((chip) => chip.textContent)).toEqual(["src/locks.ts @ abcdef1"]);

    await userEvent.click(screen.getByRole("button", { name: "Open the draft" }));
    expect(onOpenDraft).toHaveBeenCalledTimes(1);
  });

  test("keeps both existing ticket surfaces reachable", async () => {
    renderView();

    await userEvent.click(await screen.findByRole("button", { name: "Sync tickets" }));
    expect(screen.getByRole("region", { name: "Ticket sync" })).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByRole("button", { name: "Open tickets" })).toBeTruthy();
  });

  test("keeps an unnameable actor out of the byline", () => {
    // A byline names people. A decision whose actor cannot be named — no
    // recorded actor, or an account deleted before the credit was durable —
    // must not appear as one, whatever label the server gives it.
    const unknown = [
      ...decisions,
      {
        id: "d-unknown",
        kind: "section_settled" as const,
        sectionId: "s9",
        sectionTitle: "Rollout",
        actor: { id: null, name: "actor unknown" },
        decidedAt: "2026-08-13T12:00:00.000Z",
      },
    ];

    const people = decisionActors({ id: "owner", name: "Nikhil Unni" }, unknown);

    expect(people.map((person) => person.id)).toEqual(["owner", "priya"]);
    expect(people.some((person) => person.id === null)).toBe(false);
  });

  test("deduplicates the owner when the owner also decided", () => {
    expect(decisionActors({ id: "owner", name: "Nikhil Unni" }, decisions)).toEqual([
      { id: "owner", name: "Nikhil Unni" },
      { id: "priya", name: "Priya Raman" },
    ]);
    expect(provenanceSources("a.ts @ abcdef1 and a.ts @ abcdef1")).toEqual(["a.ts @ abcdef1"]);
  });
});
