/**
 * The alternatives stage in the canvas (ADR 0114 D6, requirement R20).
 *
 * Cards carry one premise and exactly three signed trade-off lines. Compare
 * replaces the cards in place and holds the numbers, under one provenance
 * caption that covers the whole surface (R16). The doc stays one scroll below.
 */

import { useState } from "react";
import { LayersIcon, ScaleIcon, ShieldCheckIcon } from "lucide-react";
import type {
  SpecAlternativeOption,
  SpecAlternativesStage,
  SpecTradeoffSign,
} from "@engrams/spec-document";

import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import "./spec-alternatives.css";

/** The sign as it is written for a person. Minus is the typographic sign. */
const TRADEOFF_MARKS: Record<SpecTradeoffSign, string> = { "+": "+", "-": "−", "~": "~" };
const TRADEOFF_LABELS: Record<SpecTradeoffSign, string> = {
  "+": "gain",
  "-": "cost",
  "~": "caveat",
};

export interface SpecAlternativesProps {
  stage: SpecAlternativesStage;
  /** Supply this only when the current user can decide. */
  editable: boolean;
  pending: boolean;
  error?: string | null;
  onPick: (input: { optionKey: string; reason: string }) => void;
  /** Ask the agent for a hybrid in the conversation. */
  onHybrid?: () => void;
}

export function SpecAlternatives({
  stage,
  editable,
  pending,
  error,
  onPick,
  onHybrid,
}: SpecAlternativesProps) {
  const [comparing, setComparing] = useState(false);
  const [pickingKey, setPickingKey] = useState<string | null>(null);
  const [reason, setReason] = useState("");
  const { proposal, decision } = stage;
  const decided = decision !== null;
  const canPick = editable && !decided;

  const startPick = (optionKey: string) => {
    setPickingKey(optionKey);
    setReason("");
  };

  return (
    <section className="spec-alternatives" aria-label="Alternatives">
      <header className="spec-alternatives-bar">
        <span className="spec-alternatives-label">
          <LayersIcon aria-hidden="true" />
          Alternatives — {comparing ? "compare" : "cards"}
        </span>
        <span className="spec-alternatives-provenance" title={proposal.comparison.provenance}>
          <ShieldCheckIcon aria-hidden="true" />
          {proposal.comparison.provenance}
        </span>
        <Button
          type="button"
          size="sm"
          variant="outline"
          className="spec-alternatives-toggle"
          onClick={() => setComparing((value) => !value)}
        >
          <ScaleIcon aria-hidden="true" />
          {comparing ? "Back to cards" : "Compare"}
        </Button>
      </header>

      {decision && (
        <p className="spec-alternatives-decided" role="status">
          <strong>
            {decision.pickedKey === null
              ? "Picked a hybrid"
              : `Picked ${decision.pickedKey} · ${titleOf(proposal.options, decision.pickedKey)}`}
          </strong>
          <span>{decision.reason}</span>
        </p>
      )}

      {comparing ? (
        <ComparisonTable stage={stage} />
      ) : (
        <div className="spec-alternatives-cards">
          {proposal.options.map((option) => (
            <OptionCard
              key={option.key}
              option={option}
              lean={proposal.leanKey === option.key}
              won={decision?.pickedKey === option.key}
              canPick={canPick}
              pending={pending}
              onPick={() => startPick(option.key)}
            />
          ))}
        </div>
      )}

      {pickingKey !== null && !decided && (
        <form
          className="spec-alternatives-reason"
          onSubmit={(event) => {
            event.preventDefault();
            if (reason.trim().length === 0) return;
            onPick({ optionKey: pickingKey, reason: reason.trim() });
          }}
        >
          <label htmlFor="spec-alternatives-reason-field">
            Why does {pickingKey} win? This is written into the section.
          </label>
          <Textarea
            id="spec-alternatives-reason-field"
            value={reason}
            rows={2}
            onChange={(event) => setReason(event.target.value)}
          />
          <div className="spec-alternatives-reason-actions">
            <Button
              type="button"
              size="sm"
              variant="ghost"
              onClick={() => setPickingKey(null)}
              disabled={pending}
            >
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={pending || reason.trim().length === 0}>
              {pending ? "Writing…" : `Confirm ${pickingKey}`}
            </Button>
          </div>
        </form>
      )}

      {error && <p className="spec-alternatives-error">{error}</p>}

      <footer className="spec-alternatives-foot">
        <span>
          {decided
            ? "The pick is written into the alternatives section."
            : `Picking writes the alternatives section — all ${proposal.options.length} options, and the reason for the winner.`}
        </span>
        {canPick && onHybrid && (
          <Button type="button" size="sm" variant="outline" onClick={onHybrid}>
            Reply with a hybrid
          </Button>
        )}
      </footer>
    </section>
  );
}

function OptionCard({
  option,
  lean,
  won,
  canPick,
  pending,
  onPick,
}: {
  option: SpecAlternativeOption;
  lean: boolean;
  won: boolean;
  canPick: boolean;
  pending: boolean;
  onPick: () => void;
}) {
  return (
    <article
      className={cardClass(lean, won)}
      data-testid="spec-alternative-card"
      aria-label={`Option ${option.key}: ${option.title}`}
    >
      <span className="spec-alternative-key">
        {option.key}
        {won ? <em>picked</em> : lean ? <em>lean</em> : null}
      </span>
      <h3>{option.title}</h3>
      <ul className="spec-alternative-tradeoffs">
        {option.tradeoffs.map((tradeoff, index) => (
          <li key={`${tradeoff.sign}-${index}`} data-testid="spec-alternative-tradeoff">
            <span className={`spec-tradeoff-sign is-${TRADEOFF_LABELS[tradeoff.sign]}`}>
              <span aria-hidden="true">{TRADEOFF_MARKS[tradeoff.sign]}</span>
              <span className="sr-only">{TRADEOFF_LABELS[tradeoff.sign]}</span>
            </span>
            {tradeoff.text}
          </li>
        ))}
      </ul>
      {canPick && (
        <Button
          type="button"
          size="sm"
          variant={lean ? "default" : "outline"}
          className="spec-alternative-pick"
          disabled={pending}
          onClick={onPick}
        >
          Pick {option.key}
        </Button>
      )}
    </article>
  );
}

function ComparisonTable({ stage }: { stage: SpecAlternativesStage }) {
  const { proposal, decision } = stage;
  const winner = decision?.pickedKey ?? proposal.leanKey;
  return (
    <table className="spec-alternatives-compare" data-testid="spec-alternatives-compare">
      <caption className="sr-only">{proposal.comparison.provenance}</caption>
      <thead>
        <tr>
          <th scope="col">Axis</th>
          {proposal.options.map((option) => (
            <th
              scope="col"
              key={option.key}
              className={option.key === winner ? "is-winner" : undefined}
            >
              {option.key} · {option.title}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {proposal.comparison.rows.map((row) => (
          <tr key={row.axis}>
            <th scope="row">{row.axis}</th>
            {proposal.options.map((option) => (
              <td key={option.key} className={option.key === winner ? "is-winner" : undefined}>
                {row.cells.find((cell) => cell.optionKey === option.key)?.value ?? "—"}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function cardClass(lean: boolean, won: boolean): string {
  if (won) return "spec-alternative-card is-won";
  return lean ? "spec-alternative-card is-lean" : "spec-alternative-card";
}

function titleOf(options: readonly SpecAlternativeOption[], key: string): string {
  return options.find((option) => option.key === key)?.title ?? key;
}
