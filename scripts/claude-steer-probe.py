#!/usr/bin/env python3
"""Steering probe for the pinned claude CLI (ADR 0052, 2026-08-20 update).

The harness's write-through steering rests on a measured CLI behavior:
a `user` message written to stdin while a TOOL CALL is in flight is
attended at the next agentic-loop step boundary, INSIDE the running
turn. The 2.1.179-era measurement ("mid-turn messages buffer to the
next turn") probed a single-generation turn — no step boundary — and
was over-generalized; the CLI drifting under a pin with no engrams code
change is a proven failure mode (the 2.1.187 AskUserQuestion removal,
engrams#431). This probe pins the behavior empirically.

Run it against the EXACT binary a pin bump names (the CLAUDE_VERSION
sites list this probe in their bump procedure):

    python3 scripts/claude-steer-probe.py [path-to-claude-binary]

Needs a logged-in claude (or ANTHROPIC_API_KEY) — it makes ~2 real
model calls, so it runs on the bumper's machine, not in CI.

Verdict (exit code):
  0 STEER  — the injected instruction was attended within the same
             turn (result #1 is the injected reply).
  1 BUFFER — the turn ran to completion and the injection ran as the
             next turn (the pre-2026-08 assumption).
  2 INCONCLUSIVE — neither shape observed (timeout / tool never ran).
"""

import json
import subprocess
import sys
import threading
import time

MARKER = "RUTABAGA"


def main() -> int:
    claude = sys.argv[1] if len(sys.argv) > 1 else "claude"
    cmd = [
        claude, "--print",
        "--input-format", "stream-json",
        "--output-format", "stream-json",
        "--verbose",
        "--allowedTools", "Bash(sleep:*),Bash(echo:*)",
    ]
    proc = subprocess.Popen(
        cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True, bufsize=1,
    )
    start = time.time()
    results: list[str] = []
    tool_started = threading.Event()
    done = threading.Event()

    def emit(tag: str, text: str) -> None:
        print(f"[{time.time() - start:7.3f}] {tag}: {text}", flush=True)

    def reader() -> None:
        for line in proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            ty = ev.get("type")
            if ty == "assistant":
                for c in ev.get("message", {}).get("content", []):
                    if c.get("type") == "tool_use":
                        emit("TOOL-USE", str(c.get("name")))
                        tool_started.set()
            elif ty == "result":
                text = str(ev.get("result", ""))
                results.append(text)
                emit("RESULT", f"#{len(results)} {text[:100]!r}")
                if len(results) >= 2 or (len(results) == 1 and MARKER in text):
                    done.set()

    threading.Thread(target=reader, daemon=True).start()

    def send(text: str) -> None:
        proc.stdin.write(json.dumps(
            {"type": "user", "message": {"role": "user", "content": text}}) + "\n")
        proc.stdin.flush()

    send("Run `sleep 8` with the Bash tool, then run `echo done` with "
         "Bash, then tell me a fun fact about turtles.")
    emit("SENT", "turn-1 tool-loop prompt")
    if not tool_started.wait(timeout=60):
        emit("VERDICT", "INCONCLUSIVE (tool call never started)")
        proc.terminate()
        return 2
    time.sleep(2.0)  # solidly inside the sleep 8
    send(f"STOP. Skip the echo and the turtle fact entirely. "
         f"End your turn replying with exactly one word: {MARKER}")
    emit("SENT", "mid-tool injection")

    done.wait(timeout=120)
    time.sleep(2.0)  # allow a trailing second result to land
    proc.terminate()

    if results and MARKER in results[0]:
        emit("VERDICT", "STEER — injection attended within the running turn")
        return 0
    if len(results) >= 2 and MARKER in results[1]:
        emit("VERDICT", "BUFFER — injection ran as the next turn")
        return 1
    emit("VERDICT", f"INCONCLUSIVE (results={results!r})")
    return 2


if __name__ == "__main__":
    sys.exit(main())
