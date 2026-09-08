import { Link } from "@tanstack/react-router";

import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { RunStatusDot } from "./RunsTab";
import { formatDuration, parseJsonObject, runStatusLabel, type TimelineStep } from "./run-format";

export interface StepRunDrawerProps {
  step: TimelineStep | null;
  blockType?: string;
  onClose(): void;
}

function JsonBlock({ title, text }: { title: string; text: string }) {
  const parsed = parseJsonObject(text);
  const pretty = parsed ? JSON.stringify(parsed, null, 2) : text;
  return (
    <section className="flex flex-col gap-1">
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
    <section className="flex flex-col gap-1">
      <h4 className="text-xs font-semibold text-muted-foreground">{title}</h4>
      <pre className="max-h-64 overflow-auto whitespace-pre-wrap rounded-md bg-secondary p-3 font-mono text-xs">
        {text}
      </pre>
    </section>
  );
}

export function StepRunDrawer({ step, blockType, onClose }: StepRunDrawerProps) {
  const latest = step?.latest;
  const outputs = latest ? parseJsonObject(latest.outputsJson) : null;
  const stdout = typeof outputs?.["stdout"] === "string" ? (outputs["stdout"] as string) : "";
  const stderr = typeof outputs?.["stderr"] === "string" ? (outputs["stderr"] as string) : "";
  const sessionId =
    latest?.sessionId ??
    (typeof outputs?.["session_id"] === "string" ? (outputs["session_id"] as string) : undefined);

  return (
    <Sheet open={step !== null} onOpenChange={(open) => !open && onClose()}>
      <SheetContent className="flex w-full flex-col gap-4 overflow-y-auto sm:max-w-xl">
        {step && latest && (
          <>
            <SheetHeader>
              <SheetTitle className="flex items-center gap-2">
                <RunStatusDot status={latest.status} />
                {step.blockId}
                {blockType && (
                  <span className="text-sm font-normal text-muted-foreground">{blockType}</span>
                )}
              </SheetTitle>
              <SheetDescription className="flex flex-wrap gap-x-4 gap-y-1 text-xs">
                <span className="capitalize">{runStatusLabel(latest.status)}</span>
                <span>{formatDuration(latest.startedAt, latest.endedAt)}</span>
                <span data-testid="retry-count">
                  {step.attempts.length === 1
                    ? "1 attempt"
                    : `${step.attempts.length} attempts (${step.attempts.length - 1} retried)`}
                </span>
                <span className="font-mono">{step.path}</span>
              </SheetDescription>
            </SheetHeader>

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

            <JsonBlock title="Inputs" text={latest.inputsJson} />
            <JsonBlock title="Outputs" text={latest.outputsJson} />
            <TextBlock title="stdout" text={stdout} />
            <TextBlock title="stderr" text={stderr} />
          </>
        )}
      </SheetContent>
    </Sheet>
  );
}
