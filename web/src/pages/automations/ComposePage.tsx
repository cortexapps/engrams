/** The "New automation" front door (Builder v2): describe the workflow in
 * plain English; a drafting agent recons the repo and the org's automations,
 * then assembles a validated draft in the Builder while you watch — with
 * "build by hand" as the escape hatch to the blank editor.
 *
 * The idempotency key follows the NewSpecPage pattern: it regenerates
 * whenever the composed request changes, so a double-click replays the same
 * create and an edited prompt mints a fresh draft.
 */

import { useMutation } from "@connectrpc/connect-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { toast } from "sonner";

import { TaskComposer, type TaskComposerState } from "@/components/composer/TaskComposer";
import { draftAutomation } from "@/gen/engram/app/v1/automation-AutomationService_connectquery";

const SUGGESTIONS = [
  "When a PR opens, run the tests and page #eng-alerts if they fail",
  "Triage nightly CI failures into a session that files issues",
  "Post a Slack digest of merged PRs at 9:00 every weekday",
  "When a Linear ticket is tagged 'engrams', spawn a session to work it",
];

export function ComposePage() {
  const navigate = useNavigate();
  const draft = useMutation(draftAutomation);
  const [composerState, setComposerState] = useState<TaskComposerState | null>(null);
  const [prefill, setPrefill] = useState<string | null>(null);

  const signature = JSON.stringify([
    composerState?.prompt ?? "",
    composerState?.profileId ?? "",
    composerState?.harnessOverride.harness ?? null,
    composerState?.harnessOverride.model ?? null,
    composerState?.harnessOverride.modelRouter ?? null,
    composerState?.harnessOverride.effort ?? null,
    composerState?.harnessOverride.mode ?? null,
  ]);
  const [idempotencyKey, setIdempotencyKey] = useState(() => crypto.randomUUID());
  useEffect(() => setIdempotencyKey(crypto.randomUUID()), [signature]);

  const submit = async (state: TaskComposerState) => {
    if (!state.valid || !state.profileId || draft.isPending) return;
    const { harnessOverride } = state;
    try {
      const result = await draft.mutateAsync({
        prompt: state.prompt,
        profileId: state.profileId,
        idempotencyKey,
        ...(harnessOverride.harness ? { harness: harnessOverride.harness } : {}),
        ...(harnessOverride.model ? { model: harnessOverride.model } : {}),
        ...(harnessOverride.modelRouter ? { modelRouter: harnessOverride.modelRouter } : {}),
        ...(harnessOverride.effort ? { effort: harnessOverride.effort } : {}),
        ...(harnessOverride.mode ? { harnessMode: harnessOverride.mode } : {}),
      });
      void navigate({
        to: "/automations/$id",
        params: { id: result.automationId },
        search: { tab: "build" },
      });
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "Could not start drafting");
    }
  };

  return (
    <div className="mx-auto max-w-4xl space-y-6 py-8" data-testid="automation-compose">
      <div className="space-y-2">
        <p className="text-muted-foreground text-xs font-medium tracking-wide uppercase">
          New automation
        </p>
        <h1 className="text-2xl font-semibold text-balance">What should it do?</h1>
        <p className="text-muted-foreground max-w-[56ch] text-sm leading-relaxed">
          Rough is fine. A drafting agent reads the repository and your existing automations, asks
          when a decision is yours, and assembles the draft in your Builder while you watch. Nothing
          runs until you enable it.
        </p>
      </div>

      <TaskComposer
        key={prefill ?? "blank"}
        submitLabel="Start drafting"
        pendingLabel="Starting…"
        pending={draft.isPending}
        placeholder="When a PR opens, run the tests and…"
        {...(prefill !== null ? { initialPrompt: prefill } : {})}
        onStateChange={setComposerState}
        onSubmit={submit}
        submitTestId="start-drafting"
      />

      <div className="flex flex-wrap gap-2">
        {SUGGESTIONS.map((suggestion) => (
          <button
            key={suggestion}
            type="button"
            className="text-muted-foreground hover:text-foreground hover:border-ring rounded-full border px-3 py-1 text-xs"
            onClick={() => setPrefill(suggestion)}
          >
            {suggestion}
          </button>
        ))}
      </div>

      <p className="text-muted-foreground text-xs">
        The draft lands as a disabled automation you can edit at any point.{" "}
        <Link
          to="/automations/new/manual"
          className="decoration-muted-foreground/50 underline decoration-dotted underline-offset-2"
          data-testid="build-by-hand"
        >
          or build by hand in the Builder →
        </Link>
      </p>
    </div>
  );
}
