/** "Test with sample" (ADR 0119 phase 3.4).
 *
 * Renders every block's config against the selected sample with NO side
 * effects (TestRender), shows filter/branch verdicts, and — across the recent
 * sample ledger — the "14/20 pass" tally that tells an author what a
 * predicate admits before they enable it. Server BlockErrors route through
 * the shell into the inspector. DryRun (executes for real with actions
 * stubbed) is 3.5's launcher; the `actions` slot is where it mounts. */

import { useState, type ReactNode } from "react";
import { Beaker, ChevronDown, ChevronRight } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import type {
  AutomationTestState,
  BlockRenderResult,
  FilterTally,
} from "@/hooks/useAutomationTest";

import { SamplePicker } from "./SamplePicker";

export interface TestPanelProps {
  test: AutomationTestState;
  /** 3.5 mounts the DryRun launcher here. */
  actions?: ReactNode;
  now?: Date;
}

function VerdictPill({ pass }: { pass: boolean }) {
  return (
    <Badge
      variant="outline"
      data-verdict={pass ? "pass" : "fail"}
      className={
        pass
          ? "border-[var(--instrument-nominal)] text-[var(--instrument-nominal-ink)]"
          : "border-[var(--instrument-critical)] text-[var(--instrument-critical-ink)]"
      }
    >
      {pass ? "pass" : "fail"}
    </Badge>
  );
}

function TallyPill({ tally }: { tally: FilterTally }) {
  const tone =
    tally.passed === 0 ? "critical" : tally.passed === tally.total ? "nominal" : "caution";
  return (
    <Badge
      variant="outline"
      data-tally={`${tally.passed}/${tally.total}`}
      className={`border-[var(--instrument-${tone})] text-[var(--instrument-${tone}-ink)]`}
    >
      {tally.passed}/{tally.total} pass
    </Badge>
  );
}

function BlockResult({ block, tally }: { block: BlockRenderResult; tally?: FilterTally }) {
  const [open, setOpen] = useState(false);
  return (
    <Collapsible open={open} onOpenChange={setOpen} data-testid={`test-block-${block.blockId}`}>
      <div className="flex items-center gap-2 py-1">
        <CollapsibleTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-7 gap-1 px-1 font-mono text-xs"
          >
            {open ? (
              <ChevronDown className="size-3.5" aria-hidden />
            ) : (
              <ChevronRight className="size-3.5" aria-hidden />
            )}
            {block.blockId}
          </Button>
        </CollapsibleTrigger>
        <span className="text-muted-foreground text-xs">{block.blockType}</span>
        {block.filterPass !== undefined ? <VerdictPill pass={block.filterPass} /> : null}
        {tally ? <TallyPill tally={tally} /> : null}
      </div>
      <CollapsibleContent>
        <pre className="bg-muted max-h-64 overflow-auto rounded-md p-2 font-mono text-[11px] leading-snug">
          {JSON.stringify(block.rendered, null, 2)}
        </pre>
      </CollapsibleContent>
    </Collapsible>
  );
}

export function TestPanel({ test, actions, now }: TestPanelProps) {
  const canRun = test.sample.kind !== "none" || test.isTimed;
  const tallyFor = (blockId: string) => test.tally?.find((t) => t.blockId === blockId);

  return (
    <section
      className="flex flex-col gap-3 rounded-lg border p-3"
      aria-label="Test with sample"
      data-testid="test-panel"
    >
      <header className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="flex items-center gap-2 text-sm font-medium">
          <Beaker className="size-4" aria-hidden />
          Test with sample
        </h3>
        <div className="flex items-center gap-2">
          <Button
            type="button"
            size="sm"
            variant="secondary"
            disabled={!canRun || test.running}
            onClick={() => void test.runOnce()}
          >
            Test with sample
          </Button>
          {!test.isTimed ? (
            <Button
              type="button"
              size="sm"
              variant="ghost"
              disabled={test.samples.length === 0 || test.running}
              onClick={() => void test.runAcrossSamples()}
              title="Render against every stored sample and tally each filter's verdict"
            >
              Across {test.samples.length || "recent"} samples
            </Button>
          ) : null}
          {actions}
        </div>
      </header>
      <SamplePicker
        timed={test.isTimed}
        samples={test.samples}
        loading={test.samplesLoading}
        value={test.sample}
        onChange={test.setSample}
        now={now}
      />
      {test.latest ? (
        <div className="flex flex-col" data-testid="test-results">
          {test.latest.errors.length > 0 ? (
            <p className="text-[var(--instrument-critical-ink)] text-xs">
              {test.latest.errors.length} field{test.latest.errors.length === 1 ? "" : "s"} did not
              render — highlighted in the inspector.
            </p>
          ) : null}
          {test.latest.blocks.map((block) => (
            <BlockResult key={block.blockId} block={block} tally={tallyFor(block.blockId)} />
          ))}
          {test.latest.blocks.length === 0 && test.latest.errors.length === 0 ? (
            <p className="text-muted-foreground text-xs">Nothing rendered.</p>
          ) : null}
        </div>
      ) : (
        <p className="text-muted-foreground text-xs">
          Pick a sample and run a test to see each block rendered, with filter verdicts and live
          variable previews in the inspector.
        </p>
      )}
    </section>
  );
}
