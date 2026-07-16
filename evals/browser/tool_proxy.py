#!/usr/bin/env python3
"""Bounded, JSONL-logging proxies for browser CLIs and engram-share."""

from __future__ import annotations

from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time
from urllib.request import urlopen


def append_event(event: dict[str, object]) -> None:
    path = Path(os.environ["ENGRAM_EVAL_EVENTS"])
    path.parent.mkdir(parents=True, exist_ok=True)
    event["at"] = datetime.now(timezone.utc).isoformat()
    with path.open("a", encoding="utf-8") as out:
        fcntl.flock(out, fcntl.LOCK_EX)
        out.write(json.dumps(event, separators=(",", ":")) + "\n")


def next_action() -> int:
    path = Path(os.environ["ENGRAM_EVAL_ACTION_COUNT"])
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+", encoding="utf-8") as counter:
        fcntl.flock(counter, fcntl.LOCK_EX)
        counter.seek(0)
        value = int(counter.read() or "0") + 1
        counter.seek(0)
        counter.truncate()
        counter.write(str(value))
        return value


def page_state() -> dict[str, object]:
    """Cheap observer state that does not mutate the candidate browser."""
    port = os.environ.get("ENGRAM_EVAL_CDP_PORT")
    if not port:
        return {}
    try:
        with urlopen(f"http://127.0.0.1:{port}/json", timeout=2) as response:
            pages = [
                {"url": item.get("url", ""), "title": item.get("title", "")}
                for item in json.load(response)
                if item.get("type") == "page"
            ]
        encoded = json.dumps(pages, sort_keys=True, separators=(",", ":")).encode()
        return {"pages": pages, "stateHash": hashlib.sha256(encoded).hexdigest()}
    except (OSError, ValueError):
        return {}


def share(args: list[str]) -> int:
    if any(arg in {"-h", "--help", "-V", "--version"} for arg in args):
        append_event({"type": "share_probe", "argv": args})
        print("usage: engram-share [--file] <image-or-video>")
        return 0
    append_event({"type": "share", "argv": args})
    print("shared evaluation artifact (recorded, not uploaded)")
    return 0


def introspection_block_reason(tool: str, args: list[str]) -> str | None:
    """Keep the canvas answer pixel-only across both measured browser CLIs."""
    lowered = [arg.lower() for arg in args]
    if any("/api/" in arg for arg in lowered):
        return "direct task API access is forbidden"
    if tool == "agent-browser":
        if lowered[:1] in (["eval"], ["inspect"], ["network"]):
            return f"agent-browser {lowered[0]} is forbidden in this evaluation"
        if lowered[:2] == ["get", "html"]:
            return "page-source inspection is forbidden in this evaluation"
    if tool == "playwright-cli" and lowered[:1] in (["eval"], ["run-code"]):
        return f"playwright-cli {lowered[0]} is forbidden in this evaluation"
    return None


def remove_isolation_overrides(tool: str, args: list[str]) -> list[str]:
    """Prevent candidates from leaving the episode-scoped browser session."""
    blocked = {"--session", "--namespace", "--cdp"}
    if tool == "playwright-cli":
        blocked.update({"-s"})
    cleaned: list[str] = []
    skip_value = False
    for arg in args:
        if skip_value:
            skip_value = False
            continue
        if arg in blocked:
            skip_value = True
            continue
        if any(arg.startswith(f"{option}=") for option in blocked):
            continue
        cleaned.append(arg)
    return cleaned


def browser(tool: str, args: list[str]) -> int:
    # Candidates may issue shell calls concurrently. Both clients are stateful,
    # and npx mutates a shared cache, so serialize the measured command stream.
    lock_path = Path(os.environ["ENGRAM_EVAL_BROWSER_LOCK"])
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    lock = lock_path.open("a", encoding="utf-8")
    fcntl.flock(lock, fcntl.LOCK_EX)
    pending_view = os.environ.get("ENGRAM_EVAL_PENDING_VIEW")
    if os.environ.get("ENGRAM_EVAL_VISUAL_GATE") == "1" and pending_view and Path(pending_view).exists():
        required = Path(pending_view).read_text(encoding="utf-8", errors="replace").strip()
        append_event({"type": "visual_gate", "tool": tool, "argv": args, "requiredPath": required})
        print(
            f"{tool}: call the browser_view MCP tool on {required} before another browser command",
            file=sys.stderr,
        )
        return 125
    action = next_action()
    limit = int(os.environ.get("ENGRAM_EVAL_ACTION_LIMIT", "30"))
    if action > limit:
        append_event({"type": "action_limit", "tool": tool, "argv": args, "action": action})
        print(f"{tool}: evaluation action limit ({limit}) exceeded", file=sys.stderr)
        return 124
    if reason := introspection_block_reason(tool, args):
        append_event(
            {"type": "introspection_block", "tool": tool, "argv": args, "action": action}
        )
        print(f"{tool}: {reason}; use snapshots or rendered pixels", file=sys.stderr)
        return 126

    driver_config = json.loads(
        Path(os.environ["ENGRAM_EVAL_DRIVER_CONFIG"]).read_text(encoding="utf-8")
    )
    session = os.environ["ENGRAM_EVAL_SESSION_ID"]
    args = remove_isolation_overrides(tool, args)
    command = shlex.split(str(driver_config["commands"][tool]))
    if tool == "agent-browser":
        command += [
            "--namespace",
            f"engram-eval-{session}",
            "--session",
            session,
            "--cdp",
            str(driver_config["cdpPort"]),
        ]
    else:
        command += [f"-s={session}"]
    command += args
    started = time.monotonic()
    before = page_state()
    append_event(
        {"type": "browser_start", "tool": tool, "argv": args, "action": action, "before": before}
    )
    try:
        result = subprocess.run(command, env=os.environ, timeout=35, check=False)
        code = result.returncode
    except subprocess.TimeoutExpired:
        code = 124
    append_event(
        {
            "type": "browser_end",
            "tool": tool,
            "argv": args,
            "action": action,
            "exitCode": code,
            "durationMs": round((time.monotonic() - started) * 1000),
            "after": page_state(),
        }
    )
    if (
        code == 0
        and os.environ.get("ENGRAM_EVAL_VISUAL_GATE") == "1"
        and pending_view
        and tool == "agent-browser"
        and args[:1] == ["screenshot"]
        and "--annotate" in args
    ):
        candidates = [Path(arg) for arg in args[1:] if not arg.startswith("-")]
        image = next((path.resolve() for path in reversed(candidates) if path.is_file()), None)
        if image:
            Path(pending_view).write_text(str(image), encoding="utf-8")
    return code


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: tool_proxy.py <agent-browser|playwright-cli|engram-share> [args...]", file=sys.stderr)
        return 2
    tool, args = sys.argv[1], sys.argv[2:]
    if tool == "engram-share":
        return share(args)
    if tool in {"agent-browser", "playwright-cli"}:
        return browser(tool, args)
    print(f"unknown proxy tool: {tool}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
