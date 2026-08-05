import { MarkdownText } from "@/components/assistant-ui/markdown-text";
import { ToolFallback } from "@/components/assistant-ui/tool-fallback";
import {
  ToolGroupContent,
  ToolGroupRoot,
  ToolGroupTrigger,
} from "@/components/assistant-ui/tool-group";
import { TooltipIconButton } from "@/components/assistant-ui/tooltip-icon-button";
import {
  ActionBarPrimitive,
  AuiIf,
  ComposerPrimitive,
  ErrorPrimitive,
  groupPartByType,
  MessagePrimitive,
  ThreadPrimitive,
  useAui,
  useAuiState,
} from "@assistant-ui/react";
import type { ToolCallMessagePartComponent } from "@assistant-ui/react";
import {
  ArrowDownIcon,
  CheckIcon,
  CopyIcon,
  CornerDownLeftIcon,
  Loader2Icon,
  SquareIcon,
  XIcon,
} from "lucide-react";
import type { FC } from "react";
import { ShellToolPart } from "@/components/session-thread/ShellToolPart";
import { FileChangePart } from "@/components/session-thread/FileChangePart";
import { BrowserActivityPart } from "@/components/session-thread/BrowserActivityPart";
import { TaskToolPart } from "@/components/session-thread/TaskToolPart";
import { SystemMessage } from "@/components/session-thread/SystemMessage";
import { RunFooter } from "@/components/session-thread/RunFooter";
import {
  BROWSER_ACTIVITY_TOOL,
  FILE_CHANGE_TOOL,
  SHELL_TOOL,
  TASK_TOOL,
} from "@/components/session-thread/buildMessages";
import { useSessionStatus } from "@/components/session-thread/session-status";
import { useComposerActions } from "@/components/session-thread/composer-actions";
import { isSubmitKey, useEnterToSend } from "@/hooks/useEnterToSend";
import { ModeChip } from "@/components/ModeChip";
import type { SessionState } from "@/lib/types";

// The session transcript, on assistant-ui primitives. This is NOT a chatbot:
// the stream is a server-authoritative log, so the chat-only affordances the
// template ships with (edit / branch / reload / suggestions / attachments)
// are stripped. What's left: a message list with three registers (user turn,
// assistant turn with inline tool calls + a run-receipt footer, and the
// harness "system" register), plus a composer that pokes the sandbox.

export const Thread: FC = () => {
  return (
    <ThreadPrimitive.Root
      className="aui-root aui-thread-root @container flex h-full flex-col bg-background"
      style={{ ["--thread-max-width" as string]: "44rem" }}
    >
      <ThreadPrimitive.Viewport className="relative flex flex-1 flex-col overflow-x-auto overflow-y-scroll scroll-smooth">
        <div className="mx-auto flex w-full max-w-(--thread-max-width) flex-1 flex-col px-4 pt-4">
          <AuiIf condition={(s) => s.thread.isEmpty}>
            <ThreadEmpty />
          </AuiIf>

          <div className="mb-8 flex flex-col gap-y-6 empty:hidden">
            <ThreadPrimitive.Messages>{() => <ThreadMessage />}</ThreadPrimitive.Messages>
          </div>

          <ThreadPrimitive.ViewportFooter className="sticky bottom-0 mt-auto flex flex-col gap-2 bg-background pb-4">
            <ThreadScrollToBottom />
            <Composer />
          </ThreadPrimitive.ViewportFooter>
        </div>
      </ThreadPrimitive.Viewport>
    </ThreadPrimitive.Root>
  );
};

const ThreadMessage: FC = () => {
  const role = useAuiState((s) => s.message.role);
  // ADR 0028 A.log: messages tombstoned by a rung-1 rewind stay viewable but
  // greyed behind a left rule — the recovery is honest, not a silent deletion.
  const rewound = useAuiState((s) => s.message.metadata.custom?.rewound === true);
  // Phase 1b: an optimistic / still-queued user prompt — greyed until the
  // server consumes it (its run_started lands). It's never "lost" between
  // pressing Enter and landing authoritatively in the conversation log;
  // it shows here greyed and transitions in place to solid on consumption.
  const pending = useAuiState((s) => s.message.metadata.custom?.pending === true);
  // ADR 0108: a durable user echo whose consuming run_started has not landed
  // yet (delivery gap) — pending grey plus a "delivering…" caption.
  const delivering = useAuiState((s) => s.message.metadata.custom?.delivering === true);
  const inner =
    role === "system" ? (
      <SystemMessage />
    ) : role === "user" ? (
      <UserMessage />
    ) : (
      <AssistantMessage />
    );
  if (rewound) {
    return (
      <div
        className="border-l-2 border-muted-foreground/40 pl-3 opacity-45"
        title="Rolled back by a checkpoint recovery"
      >
        {inner}
      </div>
    );
  }
  if (pending) {
    return (
      <div
        className="opacity-50 transition-opacity"
        title={
          delivering
            ? "Delivering — the run has not started yet"
            : "Pending — not yet in the conversation log"
        }
      >
        {inner}
        {delivering && (
          <p className="mt-1 text-right text-xs text-muted-foreground italic">delivering…</p>
        )}
      </div>
    );
  }
  return inner;
};

const ThreadEmpty: FC = () => {
  return (
    <div className="my-12 text-center font-display text-sm text-muted-foreground italic">
      No activity yet.
    </div>
  );
};

const ThreadScrollToBottom: FC = () => {
  return (
    <ThreadPrimitive.ScrollToBottom asChild>
      <TooltipIconButton
        tooltip="Scroll to bottom"
        variant="outline"
        className="absolute -top-10 z-10 self-center rounded-full p-4 disabled:invisible"
      >
        <ArrowDownIcon />
      </TooltipIconButton>
    </ThreadPrimitive.ScrollToBottom>
  );
};

// The trailing "working" indicator — emitted by GroupedParts (indicator
// mode "empty") only while a message is running with no parts yet, i.e. the
// gap between sending a prompt and the first event. Once any part exists it
// renders its own running state, so this stays quiet.
const WorkingIndicator: FC = () => {
  return (
    <span className="flex items-center gap-2 text-sm text-muted-foreground">
      <Loader2Icon className="size-3.5 animate-spin" />
      <span className="animate-pulse">working…</span>
    </span>
  );
};

const AssistantMessage: FC = () => {
  return (
    <MessagePrimitive.Root
      data-role="assistant"
      className="animate-in fade-in slide-in-from-bottom-1 relative duration-150"
    >
      <div className="leading-relaxed text-foreground wrap-break-word">
        {/* Flex `gap` (not per-part margins) spaces the top-level nodes: it
            applies only BETWEEN them, never at the message's outer edges, so a
            leading tool card / group doesn't stack its margin on top of the
            inter-message gap. */}
        <div className="flex flex-col gap-5">
          <MessagePrimitive.GroupedParts
            // Coalesce adjacent tool calls into a `group-tool` node; everything
            // else stays ungrouped and renders in place.
            groupBy={groupPartByType({ "tool-call": ["group-tool"] })}
            // Match the previous Empty-slot behaviour: working indicator only
            // when the running message has no parts yet.
            indicator="empty"
          >
            {({ part, children }) => {
              switch (part.type) {
                case "group-tool":
                  // A single call isn't worth a disclosure — render it inline.
                  if (part.indices.length === 1) return children;
                  return (
                    <ToolGroupRoot>
                      <ToolGroupTrigger
                        count={part.indices.length}
                        active={part.status.type === "running"}
                      />
                      <ToolGroupContent>{children}</ToolGroupContent>
                    </ToolGroupRoot>
                  );
                case "text":
                  return <MarkdownText />;
                case "tool-call": {
                  // ShellToolPart declares concrete `ShellArgs` while the
                  // enriched part carries the generic JSON arg bag; widen to
                  // the shared component type (the same assignability the old
                  // `tools.by_name` registration relied on) so neither branch
                  // needs a value cast. Both renderers read args defensively.
                  const Tool: ToolCallMessagePartComponent =
                    part.toolName === SHELL_TOOL
                      ? ShellToolPart
                      : part.toolName === BROWSER_ACTIVITY_TOOL
                        ? BrowserActivityPart
                        : part.toolName === FILE_CHANGE_TOOL
                          ? FileChangePart
                          : part.toolName === TASK_TOOL
                            ? TaskToolPart
                            : ToolFallback;
                  return <Tool {...part} />;
                }
                case "indicator":
                  return <WorkingIndicator />;
                default:
                  return null;
              }
            }}
          </MessagePrimitive.GroupedParts>
        </div>
        <MessagePrimitive.Error>
          <ErrorPrimitive.Root className="mt-2 rounded-md border border-destructive bg-destructive/10 p-3 text-sm text-destructive dark:bg-destructive/5 dark:text-red-200">
            <ErrorPrimitive.Message className="line-clamp-2" />
          </ErrorPrimitive.Root>
        </MessagePrimitive.Error>
      </div>

      {/* min-h-6 reserves the action-bar's height so hovering (which mounts
          the autohidden Copy button) doesn't reflow the row. */}
      <div className="mt-1 flex min-h-6 items-center gap-2">
        <RunFooter />
        <AssistantActionBar />
      </div>
    </MessagePrimitive.Root>
  );
};

const AssistantActionBar: FC = () => {
  return (
    <ActionBarPrimitive.Root
      hideWhenRunning
      autohide="not-last"
      className="flex text-muted-foreground"
    >
      <ActionBarPrimitive.Copy asChild>
        <TooltipIconButton tooltip="Copy" className="size-6 [&_svg]:size-3.5">
          <AuiIf condition={(s) => s.message.isCopied}>
            <CheckIcon />
          </AuiIf>
          <AuiIf condition={(s) => !s.message.isCopied}>
            <CopyIcon />
          </AuiIf>
        </TooltipIconButton>
      </ActionBarPrimitive.Copy>
    </ActionBarPrimitive.Root>
  );
};

const UserMessage: FC = () => {
  return (
    <MessagePrimitive.Root
      data-role="user"
      className="animate-in fade-in slide-in-from-bottom-1 flex justify-end duration-150"
    >
      <div className="max-w-[80%] rounded-2xl bg-muted px-4 py-2.5 text-foreground wrap-break-word">
        <MessagePrimitive.Parts />
      </div>
    </MessagePrimitive.Root>
  );
};

const COMPOSER_BANNER: Partial<Record<SessionState, string>> = {
  dead: "This task is dead — fork it to continue.",
  completed: "This task is completed — fork it to continue.",
  failed: "This task failed during create — start a new one.",
  host_lost:
    "The host running this task went away — waiting for the reconciler to resolve to idle (resumable) or dead.",
};

const COMPOSER_HINT: Partial<Record<SessionState, string>> = {
  parked: "Task is parked — sending wakes it in about a second.",
  idle: "Task is idle — sending will resume it.",
  created: "Task is still starting up — the harness will be ready in a moment.",
};

// The queued-message rail (ADR 0052): a message sent while a run is in flight
// sits HERE — just above the input, Claude-Code style — not inline in the
// transcript, until its run starts (then it joins the conversation at the
// consumption point, in the right order). Each row shows the full text; × cancels
// it (DequeueQueued), and ↑ on an empty composer recalls the newest for editing.
const QueuedRail: FC<{
  items: { promptId: string; text: string }[];
  onRemove: (promptId: string) => void;
}> = ({ items, onRemove }) => {
  if (items.length === 0) return null;
  return (
    <div className="flex flex-col gap-1" aria-label="Queued messages">
      {items.map((item) => (
        <div
          key={item.promptId}
          className="group flex items-center gap-2 rounded-lg bg-muted/50 py-1 pr-1 pl-2.5 text-sm text-muted-foreground"
        >
          <CornerDownLeftIcon className="size-3 shrink-0 opacity-40" />
          <span className="min-w-0 flex-1 truncate" title={item.text}>
            {item.text}
          </span>
          <TooltipIconButton
            tooltip="Cancel"
            side="left"
            type="button"
            variant="ghost"
            size="icon"
            className="size-6 shrink-0 text-muted-foreground/50 hover:text-foreground"
            aria-label="Cancel queued message"
            onClick={() => onRemove(item.promptId)}
          >
            <XIcon className="size-3.5" />
          </TooltipIconButton>
        </div>
      ))}
    </div>
  );
};

// ADR 0107: mid-session the composer offers the one alternate mode plan mode.
// (The start screen drives its chip from the harness descriptor; a live session
// has no catalog fetch, and the coordinator rejects a mode the harness never
// declared, so an undeclared plan mode fails loudly rather than silently.)
const PLAN_MODE = [{ id: "plan", label: "Plan" }];

const Composer: FC = () => {
  const status = useSessionStatus();
  const banner = status ? COMPOSER_BANNER[status] : undefined;
  const hint = status ? COMPOSER_HINT[status] : undefined;

  // ADR 0052: the composer drives submit/interrupt itself (via
  // ComposerActionsContext), NOT assistant-ui's run-gated Send/Cancel — so a
  // prompt can be ENQUEUED while a run is in flight (type-ahead), ⌘↵ works
  // mid-run, and Esc interrupts. `submitMode="none"` disables the primitive's
  // own keyboard submit so plain Enter stays a newline and our keydown owns ⌘↵.
  const {
    submit,
    interrupt,
    sendBlocked,
    canRecall,
    recall,
    queued,
    removeQueued,
    mode,
    setMode,
    planPending,
  } = useComposerActions();
  const planMode = mode === "plan";
  const isRunning = useAuiState((s) => s.thread.isRunning);
  const composer = useAui().composer;
  const text = useAuiState((s) => s.composer.text);
  const isEmpty = text.trim().length === 0;
  // Slack-style toggle: when on, plain ↵ sends and ⇧↵ is the newline. See
  // useEnterToSend + the composer settings toggle in ProfilePanel.
  const [enterToSend] = useEnterToSend();

  if (banner) {
    return (
      <div className="rounded-lg border border-dashed px-4 py-3 text-sm text-muted-foreground italic">
        {banner}
      </div>
    );
  }

  // While a run is in flight, "esc to interrupt" is ALWAYS shown (the user
  // asked to keep that affordance regardless of text). ↑-recall is offered
  // only on an empty composer with something still queued. Both can coexist.
  const hints: string[] = [];
  if (planPending) hints.push("the agent proposed a plan — review it above ↑");
  if (canRecall && isEmpty) hints.push("↑ to edit queued message");
  if (isRunning) hints.push("esc to interrupt");
  if (planMode) hints.push("plan mode — a read-only design pass");
  const hintLine = hints.length ? hints.join(" · ") : hint;

  return (
    <ComposerPrimitive.Root className="relative flex w-full flex-col gap-1.5">
      <QueuedRail items={queued} onRemove={removeQueued} />
      <div className="flex w-full items-end gap-2 rounded-2xl border bg-background p-2 transition-shadow focus-within:ring-2 focus-within:ring-ring/20">
        <ComposerPrimitive.Input
          // Default (Enter-to-send on, like Claude desktop): plain ↵ submits,
          // ⇧↵ is the newline. With the preference off it reverts to the
          // writing-surface model — Enter inserts a newline and ⌘/Ctrl+Enter
          // submits. submitMode="none" leaves submit entirely to our keydown so
          // it isn't run-gated.
          submitMode="none"
          // ADR 0108: the library default is a DOCUMENT-level capture-phase
          // Esc handler with no isRunning gate — it fired a phantom interrupt
          // from an idle page and double-fired next to our own Esc handler
          // below. Our keydown handler is the single Esc path.
          cancelOnEscape={false}
          placeholder={
            planMode
              ? "Describe what to plan — the agent explores, it won't edit…"
              : enterToSend
                ? "Reply to the task…   (↵ to send, ⇧↵ for newline)"
                : "Reply to the task…   (⌘↵ to send)"
          }
          className="max-h-40 min-h-9 flex-1 resize-none bg-transparent px-2 py-1.5 text-sm outline-none placeholder:text-muted-foreground/80"
          rows={1}
          aria-label="Message input"
          onKeyDown={(e) => {
            if (isSubmitKey(e, enterToSend)) {
              e.preventDefault();
              const t = text.trim();
              if (t && !sendBlocked) {
                submit(t);
                composer.setText("");
              }
              return;
            }
            // Esc interrupts the in-flight run, regardless of composer text. A
            // queued message (if any) then auto-runs next per the harness's
            // consume-on-result.
            if (e.key === "Escape" && isRunning) {
              e.preventDefault();
              interrupt("esc");
              return;
            }
            // Plain ↑ on an empty composer recalls the newest queued message.
            // Any modifier or existing text falls through to normal caret nav.
            if (
              e.key === "ArrowUp" &&
              !e.shiftKey &&
              !e.metaKey &&
              !e.ctrlKey &&
              !e.altKey &&
              canRecall &&
              isEmpty
            ) {
              const recalled = recall();
              if (recalled != null) {
                e.preventDefault();
                composer.setText(recalled);
              }
            }
          }}
        />
        <ModeChip
          modes={PLAN_MODE}
          value={planMode ? "plan" : null}
          onChange={(next) => setMode(next ?? "default")}
        />
        <ComposerAction />
      </div>
      {hintLine && <p className="mt-1.5 px-2 text-xs text-muted-foreground italic">{hintLine}</p>}
    </ComposerPrimitive.Root>
  );
};

const ComposerAction: FC = () => {
  const { submit, interrupt, sendBlocked } = useComposerActions();
  const isRunning = useAuiState((s) => s.thread.isRunning);
  const composer = useAui().composer;
  const text = useAuiState((s) => s.composer.text);
  const isEmpty = text.trim().length === 0;
  const [enterToSend] = useEnterToSend();
  // The button mirrors the active send chord: ↵ on its own vs ⌘↵.
  const sendChord = enterToSend ? "↵" : "⌘↵";

  // Mouse-only users keep their Stop button: while a run is in flight AND the
  // composer is empty, show Stop. The instant the user types (intent = queue a
  // message), it flips to the ⌘↵ Send/Queue button; clearing the text flips it
  // back. (The "esc to interrupt" hint stays up the whole time regardless.)
  if (isRunning && isEmpty) {
    return (
      <TooltipIconButton
        tooltip="Stop"
        side="bottom"
        type="button"
        variant="default"
        size="icon"
        className="size-8 rounded-full"
        aria-label="Stop the run"
        onClick={() => interrupt("stop-button")}
      >
        <SquareIcon className="size-3 fill-current" />
      </TooltipIconButton>
    );
  }

  return (
    <TooltipIconButton
      tooltip={`${isRunning ? "Queue" : "Send"} (${sendChord})`}
      side="bottom"
      type="button"
      variant="default"
      size="icon"
      className="h-8 w-auto gap-0.5 rounded-full px-3 font-mono text-xs"
      aria-label={isRunning ? "Queue message" : "Send message"}
      disabled={isEmpty || sendBlocked}
      onClick={() => {
        const t = text.trim();
        if (!t || sendBlocked) return;
        submit(t);
        composer.setText("");
      }}
    >
      {!enterToSend && (
        <span aria-hidden className="leading-none">
          ⌘
        </span>
      )}
      <CornerDownLeftIcon className="size-3.5" />
    </TooltipIconButton>
  );
};
