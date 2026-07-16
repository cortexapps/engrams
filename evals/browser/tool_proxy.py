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


def page_state(*, settle: bool = False) -> dict[str, object]:
    """Hash visible task state without exposing it to the candidate."""
    port = os.environ.get("ENGRAM_EVAL_CDP_PORT")
    if not port:
        return {}
    # The task publishes its sanitized observable state with a local fetch
    # after each DOM event. Sample after that microtask/network hop so a CLI's
    # process-return speed cannot change no-progress scoring.
    if settle:
        time.sleep(0.075)
    try:
        with urlopen(f"http://127.0.0.1:{port}/json", timeout=2) as response:
            pages = [
                {"url": item.get("url", ""), "title": item.get("title", "")}
                for item in json.load(response)
                if item.get("type") == "page"
            ]
        observable: object = {}
        state_url = os.environ.get("ENGRAM_EVAL_STATE_URL")
        if state_url:
            with urlopen(state_url, timeout=2) as response:
                observable = json.load(response)
        encoded = json.dumps(
            {"pages": pages, "observable": observable},
            sort_keys=True,
            separators=(",", ":"),
        ).encode()
        return {"pages": pages, "stateHash": hashlib.sha256(encoded).hexdigest()}
    except (OSError, ValueError):
        return {}


def share(args: list[str]) -> int:
    if any(arg in {"-h", "--help", "-V", "--version"} for arg in args):
        append_event({"type": "share_probe", "argv": args})
        print("usage: engram-share [--file] <image-or-video>")
        return 0
    file_value: str | None = None
    for index, arg in enumerate(args):
        if arg == "--file" and index + 1 < len(args):
            file_value = args[index + 1]
            break
        if arg.startswith("--file="):
            file_value = arg.removeprefix("--file=")
            break
    if file_value is None:
        file_value = next((arg for arg in args if not arg.startswith("-")), None)
    valid = False
    reason = "engram-share requires --file"
    if file_value:
        path = Path(file_value).resolve()
        deliverable_value = os.environ.get("ENGRAM_EVAL_DELIVERABLE_ROOT")
        deliverable_root = Path(deliverable_value).resolve() if deliverable_value else None
        if not deliverable_root or not path.is_relative_to(deliverable_root):
            reason = "shared file must be in the deliverable directory"
        else:
            # Codex may schedule a screenshot and its share command
            # concurrently. Preserve exactly-once intent by allowing the
            # producer a short bounded window to publish the declared file.
            deadline = time.monotonic() + 2
            while not path.is_file() and time.monotonic() < deadline:
                time.sleep(0.025)
            if not path.is_file():
                reason = "shared file does not exist"
                append_event(
                    {"type": "share", "argv": args, "valid": valid, "reason": reason}
                )
                print(f"engram-share: {reason}", file=sys.stderr)
                return 2
            try:
                with path.open("rb") as shared:
                    prefix = shared.read(12)
                image = (
                    prefix.startswith(b"\x89PNG\r\n\x1a\n")
                    or prefix.startswith(b"\xff\xd8\xff")
                    or (prefix[:4] == b"RIFF" and prefix[8:12] == b"WEBP")
                )
            except OSError:
                image = False
            if not image:
                reason = "shared file is not a supported image"
            else:
                grade_url = os.environ.get("ENGRAM_EVAL_GRADE_URL")
                try:
                    with urlopen(str(grade_url), timeout=2) as response:
                        task_success = bool(json.load(response).get("success"))
                except (OSError, TypeError, ValueError):
                    task_success = False
                if not task_success:
                    reason = "task must be successful before sharing evidence"
                else:
                    valid = True
                    reason = "valid post-success evidence"
    append_event({"type": "share", "argv": args, "valid": valid, "reason": reason})
    if not valid:
        print(f"engram-share: {reason}", file=sys.stderr)
        return 2
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


def screenshot_path(tool: str, args: list[str]) -> Path | None:
    if args[:1] != ["screenshot"]:
        return None
    if tool == "playwright-cli":
        for index, arg in enumerate(args[1:], 1):
            if arg == "--filename" and index + 1 < len(args):
                return Path(args[index + 1])
            if arg.startswith("--filename="):
                return Path(arg.removeprefix("--filename="))
        return None
    candidates = [Path(arg) for arg in args[1:] if not arg.startswith("-")]
    return candidates[-1] if candidates else None


def internal_screenshot_path(tool: str, args: list[str], observation_root: Path) -> Path | None:
    candidate = screenshot_path(tool, args)
    if candidate is None or not candidate.is_file():
        return None
    image = candidate.resolve()
    return image if image.is_relative_to(observation_root.resolve()) else None


def screenshot_role(tool: str, args: list[str]) -> str | None:
    candidate = screenshot_path(tool, args)
    if candidate is None:
        return None
    image = candidate.resolve()
    for role, key in (
        ("internal", "ENGRAM_EVAL_OBSERVATION_ROOT"),
        ("deliverable", "ENGRAM_EVAL_DELIVERABLE_ROOT"),
    ):
        value = os.environ.get(key)
        if value and image.is_relative_to(Path(value).resolve()):
            return role
    return "unclassified"


def screenshot_policy_error(tool: str, args: list[str]) -> str | None:
    if args[:1] != ["screenshot"]:
        return None
    if any(arg in {"-h", "--help", "-V", "--version"} for arg in args[1:]):
        return None
    candidate = screenshot_path(tool, args)
    if candidate is None:
        return "screenshots require an explicit output path"
    image = candidate.resolve()
    observation_value = os.environ.get("ENGRAM_EVAL_OBSERVATION_ROOT")
    deliverable_value = os.environ.get("ENGRAM_EVAL_DELIVERABLE_ROOT")
    observation_root = Path(observation_value).resolve() if observation_value else None
    deliverable_root = Path(deliverable_value).resolve() if deliverable_value else None
    internal = bool(observation_root and image.is_relative_to(observation_root))
    deliverable = bool(deliverable_root and image.is_relative_to(deliverable_root))
    if not internal and not deliverable:
        return "screenshot path must be in the observation or deliverable directory"
    if internal and os.environ.get("ENGRAM_EVAL_VISUAL_GATE") != "1":
        return "internal screenshots are unavailable in this configuration"
    if deliverable and os.environ.get("ENGRAM_EVAL_EVIDENCE_REQUESTED") != "1":
        return "final evidence was not requested for this task"
    return None


def browser(tool: str, args: list[str]) -> int:
    action = next_action()
    limit = int(os.environ.get("ENGRAM_EVAL_ACTION_LIMIT", "30"))
    if action > limit:
        append_event({"type": "action_limit", "tool": tool, "argv": args, "action": action})
        print(f"{tool}: evaluation action limit ({limit}) exceeded", file=sys.stderr)
        return 124
    pending_view = os.environ.get("ENGRAM_EVAL_PENDING_VIEW")
    if os.environ.get("ENGRAM_EVAL_VISUAL_GATE") == "1" and pending_view and Path(pending_view).exists():
        required = Path(pending_view).read_text(encoding="utf-8", errors="replace").strip()
        append_event({"type": "visual_gate", "tool": tool, "argv": args, "requiredPath": required})
        print(
            f"{tool}: call the browser_view MCP tool on {required} before another browser command",
            file=sys.stderr,
        )
        return 125
    if reason := screenshot_policy_error(tool, args):
        append_event(
            {"type": "screenshot_policy", "tool": tool, "argv": args, "action": action}
        )
        print(f"{tool}: {reason}", file=sys.stderr)
        return 127
    if reason := introspection_block_reason(tool, args):
        append_event(
            {"type": "introspection_block", "tool": tool, "argv": args, "action": action}
        )
        print(f"{tool}: {reason}; use snapshots or rendered pixels", file=sys.stderr)
        return 126

    driver_config = json.loads(
        Path(os.environ["ENGRAM_EVAL_DRIVER_CONFIG"]).read_text(encoding="utf-8")
    )
    session = str(driver_config["sessionName"])
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
    role = screenshot_role(tool, args)
    append_event(
        {
            "type": "browser_start",
            "tool": tool,
            "argv": args,
            "action": action,
            "before": before,
            "screenshotRole": role,
        }
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
            "after": page_state(settle=True),
            "screenshotRole": role,
        }
    )
    image_arg = screenshot_path(tool, args)
    observation_root = os.environ.get("ENGRAM_EVAL_OBSERVATION_ROOT")
    if (
        code == 0
        and os.environ.get("ENGRAM_EVAL_VISUAL_GATE") == "1"
        and pending_view
        and observation_root
        and image_arg
    ):
        image = internal_screenshot_path(tool, args, Path(observation_root))
        if image is not None:
            Path(pending_view).write_text(str(image), encoding="utf-8")
            print(
                f"{tool}: private screenshot ready at {image}. "
                f"Call browser_view with this exact path now; browser commands are blocked "
                "until it returns the pixels."
            )
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
