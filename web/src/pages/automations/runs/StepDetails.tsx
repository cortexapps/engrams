import { Link } from "@tanstack/react-router";

import { RunStatusDot } from "./RunStatusDot";
import { formatDuration, parseJsonObject, runStatusLabel, type TimelineStep } from "./run-format";

// What one step did in one run: the sentence, the status and timing, the
// error verbatim, the session it touched, then the rendered inputs beside
// the outputs, with stdout and stderr when the block produced them. The
// details drawer and the canvas replay inspector both render this.

export interface StepDetailsProps {
  step: TimelineStep;
  blockType?: string;
  sentence?: string;
  /** Every frame the block ran in (a loop's iterations), when more than one. */
  frames?: readonly TimelineStep[];
  onPickFrame?: (path: string) => void;
}

function JsonBlock({ title, text }: { title: string; text: string }) {
  const parsed = parseJsonObject(text);
  const pretty = parsed ? JSON.stringify(parsed, null, 2) : text;
  return (
    <section className="flex min-w-0 flex-col gap-1">
      <h4 className="text-xs font-semibold text-muted-foreground">{title}</h4>
      <pre className="max-h-64 overflow-auto rounded-md bg-secondary p-3 font-mono text-xs">
        {pretty || "—"}
      </pre>
    </section>
  );
}

function TextBlock({ title, text }: { title: string; text: string }) {
  if (!text) return null;
  return (
    <section className="flex min-w-0 flex-col gap-1">
      <h4 className="text-xs font-semibold text-muted-foreground">{title}</h4>
      <pre className="max-h-64 overflow-auto whitespace-pre-wrap rounded-md bg-secondary p-3 font-mono text-xs">
        {text}
      </pre>
    </section>
  );
}

export function StepDetails({ step, blockType, sentence, frames, onPickFrame }: StepDetailsProps) {
  const { latest } = step;
  const outputs = parseJsonObject(latest.outputsJson);
  const stdout = typeof outputs?.["stdout"] === "string" ? (outputs["stdout"] as string) : "";
  const stderr = typeof outputs?.["stderr"] === "string" ? (outputs["stderr"] as string) : "";
  const sessionId =
    latest.sessionId ??
    (typeof outputs?.["session_id"] === "string" ? (outputs["session_id"] as string) : undefined);

  return (
    <div className="flex flex-col gap-4" data-testid="step-details">
      <div className="flex flex-col gap-1">
        <div className="flex items-center gap-2 text-sm font-semibold">
          <RunStatusDot status={latest.status} />
          <span className="truncate">{step.blockId}</span>
          {blockType && (
            <span className="font-mono text-2xs font-normal text-muted-foreground">
              {blockType}
            </span>
          )}
        </div>
        <div className="flex flex-wrap gap-x-4 gap-y-1 font-mono text-2xs text-muted-foreground">
          <span className="capitalize">{runStatusLabel(latest.status)}</span>
          <span>{formatDuration(latest.startedAt, latest.endedAt)}</span>
          <span data-testid="retry-count">
            {step.attempts.length === 1
              ? "1 attempt"
              : `${step.attempts.length} attempts (${step.attempts.length - 1} retried)`}
          </span>
          <span>{step.path}</span>
        </div>
      </div>

      {frames && frames.length > 1 && onPickFrame && (
        <label className="flex items-center gap-2 text-xs">
          <span className="text-muted-foreground">Ran {frames.length} times; showing</span>
          <select
            className="rounded-md border bg-background px-2 py-1 font-mono text-xs"
            value={step.path}
            onChange={(e) => onPickFrame(e.target.value)}
            aria-label="Iteration"
          >
            {frames.map((frame) => (
              <option key={frame.path} value={frame.path}>
                {frame.path}
              </option>
            ))}
          </select>
        </label>
      )}

      {sentence && !latest.error && <p className="text-sm">{sentence}</p>}
      {latest.error && (
        <p
          role="alert"
          className="rounded-md border border-instrument-critical/40 bg-instrument-critical/5 p-3 text-sm"
        >
          {latest.error}
        </p>
      )}
      {sessionId && (
        <Link
          to="/sessions/$id"
          params={{ id: sessionId }}
          className="text-sm underline underline-offset-2"
          data-testid="session-link"
        >
          Open session {sessionId.slice(0, 8)}
        </Link>
      )}

      <JsonBlock title="Inputs, as rendered" text={latest.inputsJson} />
      <JsonBlock title="Outputs" text={latest.outputsJson} />
      <TextBlock title="stdout" text={stdout} />
      <TextBlock title="stderr" text={stderr} />
    </div>
  );
}
