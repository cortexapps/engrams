// The guest-side contract for tailing an agent Bash command.
//
// The harness hook bridge (`BASH_TAIL_DIR` in
// crates/engram-harness-claude/src/main.rs) tees every Bash tool call's
// stdout+stderr into `/tmp/engram-bash/<tool_call_id>.log` and records the
// tee's pid in `<tool_call_id>.pid` — the tee outlives the wrapper shell
// while any background child it orphaned still writes. The web derives the
// same paths from the event's `tool_call_id` — the directory string and the
// sanitizer are a two-sided contract; change both together.

const TAIL_DIR = "/tmp/engram-bash";

/**
 * Mirror of the harness's filename sanitizer: keep only `[A-Za-z0-9_-]`,
 * cap at 128 chars. This is what makes interpolation into shell text inert.
 * Returns null when nothing survives (no log exists for such a call either).
 */
export function tailFileId(toolCallId: string): string | null {
  const id = toolCallId.replace(/[^A-Za-z0-9_-]/g, "").slice(0, 128);
  return id.length > 0 ? id : null;
}

/**
 * Command typed into a fresh guest shell to follow one Bash call's output.
 * `--pid` ends the tail when the command's tee dies — i.e. when the last
 * writer (the shell or an orphaned background child) is gone. A missing
 * pidfile falls back to a never-alive pid (kernel pid_max), so a finished
 * command's log prints once and the tail exits instead of hanging.
 */
export function bashTailCommand(toolCallId: string): string | null {
  const id = tailFileId(toolCallId);
  if (!id) return null;
  const f = `${TAIL_DIR}/${id}`;
  return (
    `if [ -f '${f}.log' ]; then ` +
    `tail -n +1 -F --pid="$(cat '${f}.pid' 2>/dev/null || echo 4194304)" '${f}.log'; ` +
    `else echo 'No output log for this command — the session image predates tailing, or the log was pruned.'; fi`
  );
}
