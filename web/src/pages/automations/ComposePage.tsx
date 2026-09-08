/** The new-automation front door. The drafting conversation stays beside the
 * request until the agent saves the first editable version. */
import { useMutation } from "@connectrpc/connect-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { Sparkles } from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";

import { TaskComposer, type TaskComposerState } from "@/components/composer/TaskComposer";
import { SessionThread } from "@/components/session-thread/SessionThread";
import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { draftAutomation } from "@/gen/engram/app/v1/automation-AutomationService_connectquery";
import { useEditorAutomation } from "@/hooks/useAutomationEditor";
import { useBuiltinAutomation, useDuplicateAutomation } from "@/hooks/useAutomations";
import { useSessionEvents } from "@/hooks/useSessionEvents";
import { errorMessage } from "@/lib/errors";

const SUGGESTIONS = [
  "When a PR opens, run the tests and page #eng-alerts if they fail",
  "Triage nightly CI failures into a session that files issues",
  "Post a Slack digest of merged PRs at 9:00 every weekday",
];

function DraftingThread({ sessionId }: { sessionId: string }) {
  const { events, streamingText, hasMore, loadingOlder, loadOlder, oldestIdx } =
    useSessionEvents(sessionId);

  return (
    <section
      className="overflow-hidden rounded-lg border bg-card"
      data-testid="drafting-thread"
      aria-label="Drafting agent conversation"
    >
      <div className="flex items-center gap-2 border-b px-3 py-2 text-xs font-medium text-muted-foreground">
        <Sparkles className="size-3.5" aria-hidden />
        Drafting agent
      </div>
      <div className="max-h-[520px] overflow-y-auto px-3">
        <SessionThread
          sessionId={sessionId}
          events={events}
          status={undefined}
          streamingText={streamingText}
          transcriptWindow={{ hasMore, loadingOlder, loadOlder, oldestIdx }}
        />
      </div>
    </section>
  );
}

export function ComposePage() {
  const navigate = useNavigate();
  const draft = useMutation(draftAutomation);
  const builtin = useBuiltinAutomation("pr_review");
  const duplicate = useDuplicateAutomation();
  const [composerState, setComposerState] = useState<TaskComposerState | null>(null);
  const [prefill, setPrefill] = useState<string | null>(null);
  const [automationId, setAutomationId] = useState<string>();
  const editor = useEditorAutomation(automationId);
  const automation = editor.data?.automation;
  const isDrafting = automationId !== undefined;

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

  useEffect(() => {
    if (!automationId || !automation || automation.currentVersion <= 0) return;
    void navigate({
      to: "/automations/$id",
      params: { id: automationId },
      search: { tab: "build" },
    });
  }, [automation, automationId, navigate]);

  const submit = async (state: TaskComposerState) => {
    if (!state.valid || !state.profileId || draft.isPending || isDrafting) return;
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
      setAutomationId(result.automationId);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "Could not start drafting");
    }
  };

  const duplicateBuiltin = async () => {
    const source = builtin.data?.automation;
    if (!source) return;
    try {
      const result = await duplicate.mutateAsync({ automationId: source.id });
      toast.success("Duplicated — edit the copy freely");
      const id = result.automation?.id;
      if (id) {
        await navigate({
          to: "/automations/$id",
          params: { id },
          search: { tab: "build" },
        });
      }
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  return (
    <div className="mx-auto max-w-[720px] space-y-6 py-8" data-testid="automation-compose">
      <Text as="h1" variant="display" className="text-2xl">
        New automation
      </Text>

      <div inert={isDrafting} aria-disabled={isDrafting}>
        <TaskComposer
          key={prefill ?? "blank"}
          submitLabel="Draft it"
          pendingLabel={isDrafting ? "Drafting…" : "Starting…"}
          pending={draft.isPending || isDrafting}
          disabled={isDrafting}
          placeholder="When a PR opens, run the tests and…"
          {...(prefill !== null ? { initialPrompt: prefill } : {})}
          onStateChange={setComposerState}
          onSubmit={submit}
          submitTestId="start-drafting"
        />
      </div>

      <div className="flex flex-col items-start justify-between gap-3 sm:flex-row sm:items-center">
        <p className="text-xs text-muted-foreground">
          An agent reads the repo and your existing automations, then assembles a draft you can
          watch, edit and turn on. Nothing runs until you turn it on.
        </p>
        <Button variant="outline" asChild className="shrink-0">
          <Link to="/automations/new/manual" data-testid="build-by-hand">
            Build by hand
          </Link>
        </Button>
      </div>

      {automation?.draftSessionId && <DraftingThread sessionId={automation.draftSessionId} />}

      <section className="space-y-2" aria-labelledby="starting-points-heading">
        <h2 id="starting-points-heading" className="text-sm font-semibold">
          Starting points
        </h2>
        <div>
          {SUGGESTIONS.map((suggestion) => (
            <button
              key={suggestion}
              type="button"
              className="flex h-9 w-full items-center gap-2 rounded-md px-2 text-left text-sm hover:bg-accent/60 disabled:pointer-events-none disabled:opacity-50"
              onClick={() => setPrefill(suggestion)}
              disabled={isDrafting}
            >
              <span className="text-muted-foreground" aria-hidden>
                →
              </span>
              {suggestion}
            </button>
          ))}
          {builtin.data?.automation && (
            <button
              type="button"
              className="flex h-9 w-full items-center gap-2 rounded-md px-2 text-left text-sm hover:bg-accent/60 disabled:pointer-events-none disabled:opacity-50"
              onClick={duplicateBuiltin}
              disabled={duplicate.isPending || isDrafting}
            >
              <span className="text-muted-foreground" aria-hidden>
                →
              </span>
              Duplicate the built-in PR review and change the repos
            </button>
          )}
        </div>
      </section>
    </div>
  );
}
