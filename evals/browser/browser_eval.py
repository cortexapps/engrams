#!/usr/bin/env python3
"""Black-box browser capability evaluator for ADR 0097."""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import html
import json
import os
from pathlib import Path
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import Iterable, TextIO
from urllib.request import Request, urlopen

from incident_console import SEEDS, start_server, task_prompt


ROOT = Path(__file__).resolve().parents[2]
PROXY = Path(__file__).with_name("tool_proxy.py")
IMAGE_MCP = Path(__file__).with_name("image_mcp.py")
MINIWOB_DRIVER = Path(__file__).with_name("miniwob_driver.py")
ARMS = ("prod", "pw-latest", "agent-dom", "hybrid")
HARNESSES = ("codex", "claude")
MINIWOB_TASKS = (
    "click-button",
    "choose-list",
    "enter-text",
    "click-checkboxes",
    "click-menu-2",
    "use-autocomplete",
)
CODEX_MODEL = os.environ.get("ENGRAM_EVAL_CODEX_MODEL", "gpt-5.4")
CLAUDE_MODEL = os.environ.get("ENGRAM_EVAL_CLAUDE_MODEL", "sonnet")


BASELINE_INSTRUCTIONS = """Use playwright-cli to drive the live browser. Start with open, inspect the page with snapshot, interact using element references, and verify the final state. Use screenshots or recordings to show visual work and publish them with engram-share. Use only browser observations and rendered pixels: do not inspect page JavaScript, HTML source, network traffic, runtime variables, or task APIs, and do not use shell HTTP clients."""

POLICY_INSTRUCTIONS = """You have one shared live browser. Use playwright-cli for this task. Follow OBSERVE -> ACT ONCE -> WAIT -> OBSERVE -> VERIFY. Start with a compact snapshot and search/read before acting. After any mutation take a fresh snapshot; never reuse stale references. If canvas, occlusion, or layout makes semantic state insufficient, capture one screenshot with boxes. Immediately call the browser_view MCP tool with its absolute path: after taking a decision screenshot, you MUST NOT take another browser action or screenshot until browser_view has returned the pixels. Then act once and verify. Do not repeat an action against unchanged page state more than twice. Screenshots are internal observations unless the user explicitly requests evidence. Publish exactly one final screenshot only when requested; otherwise never call engram-share. Record video only when explicitly requested or when demonstrating a temporal bug. Use only browser observations and rendered pixels: do not inspect page JavaScript, HTML source, network traffic, runtime variables, or task APIs, and do not use shell HTTP clients."""

AGENT_DOM_INSTRUCTIONS = """You have one shared live browser. Use agent-browser for this task. Follow OBSERVE -> ACT ONCE -> WAIT -> OBSERVE -> VERIFY. Start with `agent-browser open URL`, then compact interactive snapshots (`agent-browser snapshot -i`). Search/read before acting, refresh the snapshot after every mutation, and never reuse stale @e refs. Do not repeat an action against unchanged page state more than twice. This arm forbids screenshots for deciding browser actions: if semantic state is insufficient, report the blocker. If and only if the user explicitly requests final evidence after a successful task, take and share exactly one final screenshot; otherwise never call engram-share. Use only browser observations: do not inspect page JavaScript, HTML source, network traffic, runtime variables, or task APIs, and do not use shell HTTP clients. The canvas decision must come from pixels; because this arm has no visual tool, report that blocker rather than bypassing it."""

HYBRID_INSTRUCTIONS = """You have one shared live browser. Use agent-browser for normal interaction. Follow OBSERVE -> ACT ONCE -> WAIT -> OBSERVE -> VERIFY. Start with `agent-browser open URL`, then compact interactive snapshots (`agent-browser snapshot -i`). Search/read before acting and refresh after every mutation; never reuse stale @e refs. If the target is canvas-based, occluded, unlabeled, visually ambiguous, or one fresh semantic recovery made no progress, capture one `agent-browser screenshot --annotate <absolute-file>`. Immediately call the browser_view MCP tool with that absolute path: after taking a decision screenshot, you MUST NOT take another browser action or screenshot until browser_view has returned the pixels. Then act once and verify. Never repeat an action against unchanged page state more than twice. Screenshots are internal observations unless the user explicitly requests evidence. Publish exactly one final screenshot with engram-share only when requested; otherwise never share. Record video only when explicitly requested or for a temporal bug. Use only browser observations and rendered pixels: do not inspect page JavaScript, HTML source, network traffic, runtime variables, or task APIs, and do not use shell HTTP clients."""


@dataclass(frozen=True)
class Episode:
    arm: str
    harness: str
    seed: int
    evidence: bool


@dataclass(frozen=True)
class MiniwobEpisode:
    arm: str
    harness: str
    task: str
    seed: int = 0


def matrix() -> Iterable[Episode]:
    for arm in ARMS:
        for harness in HARNESSES:
            for seed in range(len(SEEDS)):
                for evidence in (False, True):
                    yield Episode(arm, harness, seed, evidence)


def episode_key(result: dict[str, object]) -> tuple[int, int, int, bool]:
    return (
        ARMS.index(str(result["arm"])),
        HARNESSES.index(str(result["harness"])),
        int(result["seed"]),
        bool(result["evidence"]),
    )


def miniwob_key(result: dict[str, object]) -> tuple[int, int, int]:
    return (
        ARMS.index(str(result["arm"])),
        HARNESSES.index(str(result["harness"])),
        MINIWOB_TASKS.index(str(result["task"])),
    )


def post_json(url: str, body: dict[str, object]) -> dict[str, object]:
    req = Request(
        url,
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urlopen(req, timeout=5) as response:
        return json.load(response)


def get_json(url: str) -> dict[str, object]:
    with urlopen(url, timeout=5) as response:
        return json.load(response)


def chrome_command(profile: Path, cdp_port: int) -> list[str]:
    override = os.environ.get("ENGRAM_EVAL_CHROME")
    candidates = [
        override,
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        shutil.which("google-chrome"),
        shutil.which("chromium"),
        shutil.which("chromium-browser"),
    ]
    chrome = next((p for p in candidates if p and Path(p).exists()), None)
    if not chrome:
        raise RuntimeError("Chrome/Chromium not found; set ENGRAM_EVAL_CHROME")
    return [
        chrome,
        "--headless=new",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        f"--remote-debugging-port={cdp_port}",
        "--remote-allow-origins=*",
        f"--user-data-dir={profile}",
        "about:blank",
    ]


def free_port() -> int:
    import socket

    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def wait_cdp(port: int) -> None:
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            get_json(f"http://127.0.0.1:{port}/json/version")
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"Chrome CDP :{port} did not become ready")


def write_proxies(bin_dir: Path, arm: str) -> None:
    bin_dir.mkdir(parents=True)
    browser_tool = "playwright-cli" if arm in {"prod", "pw-latest"} else "agent-browser"
    for name in (browser_tool, "engram-share"):
        path = bin_dir / name
        path.write_text(
            "#!/bin/sh\nexec \"${PYTHON:-python3}\" "
            + json.dumps(str(PROXY))
            + " "
            + name
            + " \"$@\"\n",
            encoding="utf-8",
        )
        path.chmod(0o755)


def arm_instructions(arm: str) -> str:
    instructions = {
        "prod": BASELINE_INSTRUCTIONS,
        "pw-latest": POLICY_INSTRUCTIONS,
        "agent-dom": AGENT_DOM_INSTRUCTIONS,
        "hybrid": HYBRID_INSTRUCTIONS,
    }[arm]
    browser_tool = "playwright-cli" if arm in {"prod", "pw-latest"} else "agent-browser"
    return (
        instructions
        + f"\n\nThe measured `{browser_tool}` wrapper is already first on PATH. Invoke it only "
        f"as `{browser_tool}`; do not search for browser binaries or evaluation config, use an "
        "absolute CLI path, override its session/CDP target, or call an unwrapped browser CLI. "
        "Doing so invalidates the episode."
    )


def harness_command(
    harness: str,
    workspace: Path,
    prompt: str,
    mcp_config: Path,
    visual: bool,
    observation_root: Path,
) -> list[str]:
    if harness == "codex":
        mcp_args = json.dumps([str(IMAGE_MCP), str(workspace), str(observation_root)])
        command = [
            "codex",
            "exec",
            "--cd",
            str(workspace),
            "--skip-git-repo-check",
            "--ephemeral",
            "--ignore-user-config",
            "--sandbox",
            "danger-full-access",
            "--json",
            "--model",
            CODEX_MODEL,
        ]
        if visual:
            command += [
                "-c",
                'mcp_servers.browser_view.command="python3"',
                "-c",
                f"mcp_servers.browser_view.args={mcp_args}",
            ]
        return [*command, prompt]
    command = [
        "claude",
        "--print",
        "--no-session-persistence",
        "--permission-mode",
        "bypassPermissions",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        CLAUDE_MODEL,
    ]
    command += ["--mcp-config", str(mcp_config), "--strict-mcp-config"]
    return [*command, prompt]


def command_for_arm(arm: str) -> tuple[str, str]:
    agent = os.environ.get(
        "ENGRAM_EVAL_AGENT_BROWSER_COMMAND", "npx --yes agent-browser@0.32.0"
    )
    version = "0.1.13" if arm == "prod" else "0.1.17"
    version_key = "013" if version == "0.1.13" else "017"
    playwright = os.environ.get(
        f"ENGRAM_EVAL_PLAYWRIGHT_{version_key}_COMMAND",
        f"npx --yes @playwright/cli@{version}",
    )
    return agent, playwright


def command_version(command: str) -> str:
    words = command.split()
    try:
        result = subprocess.run(
            [*words, "--version"], capture_output=True, text=True, timeout=20, check=False
        )
        return (result.stdout or result.stderr).strip().splitlines()[0]
    except (OSError, subprocess.TimeoutExpired, IndexError):
        return "unknown"


def resolved_model(log: Path) -> str:
    if not log.exists():
        return "unknown"

    def visit(value: object) -> str | None:
        if isinstance(value, dict):
            model = value.get("model")
            if isinstance(model, str) and model:
                return model
            for child in value.values():
                found = visit(child)
                if found:
                    return found
        elif isinstance(value, list):
            for child in value:
                found = visit(child)
                if found:
                    return found
        return None

    for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            found = visit(json.loads(line))
        except json.JSONDecodeError:
            continue
        if found:
            return found
    return "unknown"


def expected_model(harness: str) -> str:
    # Claude records alias resolution in its stream. Codex's JSON event stream
    # currently omits the model, so its explicit, validated id is authoritative.
    return CODEX_MODEL if harness == "codex" else CLAUDE_MODEL


def executed_shell_commands(log: Path) -> list[str]:
    commands: list[str] = []
    if not log.exists():
        return commands
    for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue

        def visit(item: object) -> None:
            if isinstance(item, dict):
                if item.get("type") == "command_execution" and isinstance(
                    item.get("command"), str
                ):
                    commands.append(str(item["command"]))
                for child in item.values():
                    visit(child)
            elif isinstance(item, list):
                for child in item:
                    visit(child)

        visit(value)
    return commands


def protocol_violation(log: Path, raw_browser_command: str, task_url: str) -> bool:
    raw_executable = shlex.split(raw_browser_command)[0]
    task_host = task_url.removeprefix("http://").split("/", 1)[0]
    for command in executed_shell_commands(log):
        lowered = command.lower()
        if raw_executable in command or "engram_eval_driver_config" in lowered:
            return True
        if "driver.json" in lowered and any(name in lowered for name in ("cat", "jq", "rg")):
            return True
        if task_host in command and any(
            marker in lowered
            for marker in ("curl ", "wget ", "urlopen", "requests.get", "httpx.")
        ):
            return True
        if "/api/" in lowered:
            return True
    return False


def read_events(path: Path) -> list[dict[str, object]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line]


def count_shared_artifacts(events: list[dict[str, object]]) -> int:
    """Count delivery attempts, excluding CLI discovery such as --help."""
    probes = {"-h", "--help", "-V", "--version"}
    return sum(
        1
        for event in events
        if event.get("type") == "share"
        and not probes.intersection(str(arg) for arg in event.get("argv", []))
    )


def no_progress_repeats(events: list[dict[str, object]]) -> int:
    starts = [e for e in events if e.get("type") == "browser_start"]
    ends = {e.get("action"): e for e in events if e.get("type") == "browser_end"}
    repeats = 0
    previous: tuple[object, object, object] | None = None
    run = 0
    for event in starts:
        end = ends.get(event.get("action"), {})
        after = end.get("after", {}) if isinstance(end, dict) else {}
        state_hash = after.get("stateHash") if isinstance(after, dict) else None
        current = (event.get("tool"), event.get("argv"), state_hash)
        if current == previous:
            run += 1
            repeats = max(repeats, run)
        else:
            run = 0
        previous = current
    return repeats


def run_episode(episode: Episode, out_root: Path) -> dict[str, object]:
    episode_id = f"{episode.arm}-{episode.harness}-s{episode.seed}-{'evidence' if episode.evidence else 'plain'}"
    out = out_root / episode_id
    out.mkdir(parents=True, exist_ok=True)
    workspace = Path(tempfile.mkdtemp(prefix=f"engrams-{episode_id}-"))
    server, thread = start_server()
    post_json(f"{server.url}/api/reset", {"seed": episode.seed})
    cdp_port = free_port()
    profile = out / "chrome-profile"
    chrome_log = (out / "chrome.log").open("wb")
    chrome = subprocess.Popen(chrome_command(profile, cdp_port), stdout=chrome_log, stderr=subprocess.STDOUT)
    started = time.monotonic()
    try:
        wait_cdp(cdp_port)
        observation_dir = out / "observations"
        observation_dir.mkdir()
        instructions = arm_instructions(episode.arm)
        (workspace / "AGENTS.md").write_text(instructions + "\n", encoding="utf-8")
        (workspace / "CLAUDE.md").write_text(instructions + "\n", encoding="utf-8")
        bin_dir = out / "bin"
        write_proxies(bin_dir, episode.arm)
        events = out / "events.jsonl"
        action_count = out / "action-count"
        pending_view = out / "pending-view"
        agent_command, playwright_command = command_for_arm(episode.arm)
        browser_tool = "playwright-cli" if episode.arm in {"prod", "pw-latest"} else "agent-browser"
        driver_config = out / "driver.json"
        driver_config.write_text(
            json.dumps(
                {
                    "cdpPort": cdp_port,
                    "commands": {
                        browser_tool: playwright_command
                        if browser_tool == "playwright-cli"
                        else agent_command
                    },
                }
            ),
            encoding="utf-8",
        )
        config = out / "playwright.config.json"
        config.write_text(json.dumps({"browser": {"cdpEndpoint": f"http://127.0.0.1:{cdp_port}"}}))
        mcp_config = out / "mcp.json"
        visual = episode.arm in {"pw-latest", "hybrid"}
        mcp_config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "browser_view": {
                            "type": "stdio",
                            "command": "python3",
                            "args": [str(IMAGE_MCP), str(workspace), str(out)],
                        }
                    }
                    if visual
                    else {}
                }
            )
        )
        env = os.environ.copy()
        env.update(
            {
                "PATH": f"{bin_dir}{os.pathsep}{env['PATH']}",
                "ENGRAM_EVAL_EVENTS": str(events),
                "ENGRAM_EVAL_ACTION_COUNT": str(action_count),
                "ENGRAM_EVAL_BROWSER_LOCK": str(out / "browser.lock"),
                "ENGRAM_EVAL_ACTION_LIMIT": "30",
                "ENGRAM_EVAL_VISUAL_GATE": "1" if episode.arm in {"hybrid", "pw-latest"} else "0",
                "ENGRAM_EVAL_PENDING_VIEW": str(pending_view),
                "ENGRAM_EVAL_CDP_PORT": str(cdp_port),
                "ENGRAM_EVAL_DRIVER_CONFIG": str(driver_config),
                "ENGRAM_EVAL_SESSION_ID": episode_id,
                "PLAYWRIGHT_MCP_CONFIG": str(config),
            }
        )
        for key in (
            "ENGRAM_EVAL_AGENT_BROWSER_COMMAND",
            "ENGRAM_EVAL_PLAYWRIGHT_COMMAND",
            "ENGRAM_EVAL_PLAYWRIGHT_013_COMMAND",
            "ENGRAM_EVAL_PLAYWRIGHT_017_COMMAND",
        ):
            env.pop(key, None)
        prompt = (
            f"Task URL: {server.url}\n"
            f"Internal screenshot directory: {observation_dir}\n\n"
            f"{task_prompt(episode.seed, episode.evidence)}"
        )
        agent_log_path = out / "agent.jsonl"
        with agent_log_path.open("wb") as agent_log:
            try:
                result = subprocess.run(
                    harness_command(
                        episode.harness,
                        workspace,
                        prompt,
                        mcp_config,
                        visual,
                        out,
                    ),
                    cwd=workspace,
                    env=env,
                    stdout=agent_log,
                    stderr=subprocess.STDOUT,
                    timeout=180,
                    check=False,
                )
                exit_code = result.returncode
                timed_out = False
            except subprocess.TimeoutExpired:
                exit_code = 124
                timed_out = True
        grade = get_json(f"{server.url}/api/grade")
        selected_browser_command = (
            playwright_command if browser_tool == "playwright-cli" else agent_command
        )
        violated_protocol = protocol_violation(
            agent_log_path, selected_browser_command, server.url
        )
        recorded = read_events(events)
        shares = count_shared_artifacts(recorded)
        screenshots = sum(
            1
            for e in recorded
            if e.get("type") == "browser_start"
            and "screenshot" in [str(a) for a in e.get("argv", [])]
        )
        expected_shares = 1 if episode.evidence and grade["success"] else 0
        actions = int(action_count.read_text() if action_count.exists() else "0")
        max_repeat = no_progress_repeats(recorded)
        bounded = not timed_out and not violated_protocol and 0 < actions <= 30 and max_repeat <= 2
        return {
            **asdict(episode),
            "id": episode_id,
            "success": bool(grade["success"]) and shares == expected_shares and bounded,
            "taskSuccess": bool(grade["success"]),
            "shareIntentSuccess": shares == expected_shares,
            "shares": shares,
            "screenshots": screenshots,
            "actions": actions,
            "maxIdenticalRepeat": max_repeat,
            "exitCode": exit_code,
            "timedOut": timed_out,
            "protocolViolation": violated_protocol,
            "durationMs": round((time.monotonic() - started) * 1000),
            "harnessVersion": command_version(episode.harness),
            "resolvedModel": resolved_model(agent_log_path)
            if episode.harness == "claude"
            else expected_model(episode.harness),
            "browserVersion": "playwright-cli@0.1.13"
            if episode.arm == "prod"
            else (
                "playwright-cli@0.1.17"
                if episode.arm == "pw-latest"
                else "agent-browser@0.32.0"
            ),
            "grade": grade,
        }
    finally:
        chrome.send_signal(signal.SIGTERM)
        try:
            chrome.wait(timeout=5)
        except subprocess.TimeoutExpired:
            chrome.kill()
        chrome_log.close()
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
        shutil.rmtree(workspace, ignore_errors=True)


def median(values: list[int]) -> int | float | None:
    values = sorted(values)
    if not values:
        return None
    middle = len(values) // 2
    return values[middle] if len(values) % 2 else (values[middle - 1] + values[middle]) / 2


def arm_summary(results: list[dict[str, object]]) -> dict[str, dict[str, object]]:
    summary: dict[str, dict[str, object]] = {}
    for arm in ARMS:
        selected = [result for result in results if result["arm"] == arm]
        if not selected:
            continue
        summary[arm] = {
            "episodes": len(selected),
            "passes": sum(bool(result["success"]) for result in selected),
            "taskPasses": sum(bool(result["taskSuccess"]) for result in selected),
            "sharePasses": sum(bool(result["shareIntentSuccess"]) for result in selected),
            "timeouts": sum(bool(result["timedOut"]) for result in selected),
            "maxIdenticalRepeat": max(int(result["maxIdenticalRepeat"]) for result in selected),
            "medianSuccessfulCommands": median(
                [int(result["actions"]) for result in selected if bool(result["taskSuccess"])]
            ),
            "byHarness": {
                harness: sum(
                    bool(result["success"])
                    for result in selected
                    if result["harness"] == harness
                )
                for harness in HARNESSES
            },
        }
    return summary


def incident_gate(results: list[dict[str, object]]) -> dict[str, object]:
    summary = arm_summary(results)
    complete = len(results) == len(ARMS) * len(HARNESSES) * len(SEEDS) * 2
    if not complete or "prod" not in summary or "hybrid" not in summary:
        return {"evaluated": False, "reason": "requires the complete 64-episode matrix"}
    prod = summary["prod"]
    hybrid = summary["hybrid"]
    hybrid_results = [result for result in results if result["arm"] == "hybrid"]
    checks = {
        "pooledPassDeltaAtLeastThree": int(hybrid["passes"]) - int(prod["passes"]) >= 3,
        "codexLosesAtMostOne": int(hybrid["byHarness"]["codex"])
        >= int(prod["byHarness"]["codex"]) - 1,
        "claudeLosesAtMostOne": int(hybrid["byHarness"]["claude"])
        >= int(prod["byHarness"]["claude"]) - 1,
        "allWithinBounds": all(
            not bool(result["timedOut"]) and 0 < int(result["actions"]) <= 30
            for result in hybrid_results
        ),
        "noProtocolViolations": all(
            not bool(result.get("protocolViolation")) for result in hybrid_results
        ),
        "noMoreThanTwoIdenticalFailures": all(
            int(result["maxIdenticalRepeat"]) <= 2 for result in hybrid_results
        ),
        "sharingExact": all(bool(result["shareIntentSuccess"]) for result in hybrid_results),
    }
    return {"evaluated": True, "passed": all(checks.values()), "checks": checks}


def write_report(results: list[dict[str, object]], out: Path) -> None:
    # Re-grade sharing from the append-only traces. This intentionally makes
    # report generation robust to an older logging shim that classified an
    # `engram-share --help` capability probe as a delivery attempt.
    for result in results:
        events = out / str(result["id"]) / "events.jsonl"
        if events.exists():
            shares = count_shared_artifacts(read_events(events))
            result["shares"] = shares
            expected_shares = (
                1 if bool(result.get("evidence")) and bool(result["taskSuccess"]) else 0
            )
            result["shareIntentSuccess"] = shares == expected_shares
    # A timed-out or command-limit episode is a failed episode even if it
    # happened to mutate the task into the right state before being killed.
    for result in results:
        result["success"] = bool(result["taskSuccess"]) and bool(
            result["shareIntentSuccess"]
        ) and not bool(result["timedOut"]) and not bool(
            result.get("protocolViolation")
        ) and 0 < int(result["actions"]) <= 30 and int(result["maxIdenticalRepeat"]) <= 2
    (out / "results.json").write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
    summary = {
        "generatedAt": datetime.now(timezone.utc).isoformat(),
        "models": {"codex": CODEX_MODEL, "claudeRequested": CLAUDE_MODEL},
        "arms": arm_summary(results),
        "incidentGate": incident_gate(results),
    }
    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    rows = "".join(
        "<tr>"
        + "".join(
            f"<td>{html.escape(str(r[k]))}</td>"
            for k in ("id", "success", "taskSuccess", "shares", "screenshots", "actions", "durationMs")
        )
        + "</tr>"
        for r in results
    )
    report = f"""<!doctype html><meta charset=utf-8><title>Browser eval</title>
<style>body{{font:14px system-ui;margin:30px}}table{{border-collapse:collapse}}th,td{{padding:7px;border:1px solid #ccc}}</style>
<h1>Browser evaluation</h1><p>{sum(bool(r['success']) for r in results)} / {len(results)} passed</p>
<pre>{html.escape(json.dumps(summary, indent=2))}</pre>
<table><thead><tr><th>episode</th><th>pass</th><th>task</th><th>shares</th><th>screenshots</th><th>actions</th><th>ms</th></tr></thead><tbody>{rows}</tbody></table>"""
    (out / "report.html").write_text(report, encoding="utf-8")


def miniwob_paths() -> tuple[str, Path]:
    python = os.environ.get("ENGRAM_BROWSERGYM_PYTHON")
    root_value = os.environ.get("ENGRAM_MINIWOB_ROOT")
    root = Path(root_value) if root_value else None
    if not python or not Path(python).is_file() or root is None or not root.is_dir():
        raise RuntimeError(
            "MiniWoB calibration requires ENGRAM_BROWSERGYM_PYTHON and ENGRAM_MINIWOB_ROOT; "
            "see evals/browser/README.md"
        )
    return python, root


def read_line_with_timeout(stream: TextIO, timeout: float, context: str) -> str:
    ready, _, _ = select.select([stream], [], [], timeout)
    if not ready:
        raise TimeoutError(f"timed out waiting for {context}")
    line = stream.readline()
    if not line:
        raise RuntimeError(f"process exited before replying to {context}")
    return line


def driver_request(
    driver: subprocess.Popen[str], body: dict[str, object], timeout: float = 5
) -> dict[str, object]:
    assert driver.stdin is not None and driver.stdout is not None
    driver.stdin.write(json.dumps(body) + "\n")
    driver.stdin.flush()
    line = read_line_with_timeout(driver.stdout, timeout, "MiniWoB driver response")
    return json.loads(line)


def run_miniwob_episode(episode: MiniwobEpisode, out_root: Path) -> dict[str, object]:
    episode_id = f"miniwob-{episode.arm}-{episode.harness}-{episode.task}-s{episode.seed}"
    out = out_root / episode_id
    out.mkdir(parents=True, exist_ok=True)
    workspace = Path(tempfile.mkdtemp(prefix=f"engrams-{episode_id}-"))
    cdp_port = free_port()
    profile = out / "chrome-profile"
    chrome_log = (out / "chrome.log").open("wb")
    chrome = subprocess.Popen(
        chrome_command(profile, cdp_port), stdout=chrome_log, stderr=subprocess.STDOUT
    )
    driver: subprocess.Popen[str] | None = None
    driver_log = None
    started = time.monotonic()
    try:
        wait_cdp(cdp_port)
        browsergym_python, miniwob_root = miniwob_paths()
        base_url = (miniwob_root / "miniwob/html/miniwob").resolve().as_uri() + "/"
        driver_log = (out / "driver.log").open("w", encoding="utf-8")
        driver = subprocess.Popen(
            [
                browsergym_python,
                str(MINIWOB_DRIVER),
                "--cdp",
                f"http://127.0.0.1:{cdp_port}",
                "--task",
                episode.task,
                "--seed",
                str(episode.seed),
                "--base-url",
                base_url,
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=driver_log,
            text=True,
        )
        assert driver.stdout is not None
        setup_line = read_line_with_timeout(driver.stdout, 15, "MiniWoB driver setup")
        setup = json.loads(setup_line)

        instructions = arm_instructions(episode.arm)
        (workspace / "AGENTS.md").write_text(instructions + "\n", encoding="utf-8")
        (workspace / "CLAUDE.md").write_text(instructions + "\n", encoding="utf-8")
        bin_dir = out / "bin"
        write_proxies(bin_dir, episode.arm)
        events = out / "events.jsonl"
        action_count = out / "action-count"
        pending_view = out / "pending-view"
        agent_command, playwright_command = command_for_arm(episode.arm)
        browser_tool = "playwright-cli" if episode.arm in {"prod", "pw-latest"} else "agent-browser"
        driver_config = out / "driver.json"
        driver_config.write_text(
            json.dumps(
                {
                    "cdpPort": cdp_port,
                    "commands": {
                        browser_tool: playwright_command
                        if browser_tool == "playwright-cli"
                        else agent_command
                    },
                }
            ),
            encoding="utf-8",
        )
        config = out / "playwright.config.json"
        config.write_text(json.dumps({"browser": {"cdpEndpoint": f"http://127.0.0.1:{cdp_port}"}}))
        mcp_config = out / "mcp.json"
        visual = episode.arm in {"pw-latest", "hybrid"}
        mcp_config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "browser_view": {
                            "type": "stdio",
                            "command": "python3",
                            "args": [str(IMAGE_MCP), str(workspace), str(out)],
                        }
                    }
                    if visual
                    else {}
                }
            )
        )
        env = os.environ.copy()
        env.update(
            {
                "PATH": f"{bin_dir}{os.pathsep}{env['PATH']}",
                "ENGRAM_EVAL_EVENTS": str(events),
                "ENGRAM_EVAL_ACTION_COUNT": str(action_count),
                "ENGRAM_EVAL_BROWSER_LOCK": str(out / "browser.lock"),
                "ENGRAM_EVAL_ACTION_LIMIT": "30",
                "ENGRAM_EVAL_VISUAL_GATE": "1"
                if episode.arm in {"hybrid", "pw-latest"}
                else "0",
                "ENGRAM_EVAL_PENDING_VIEW": str(pending_view),
                "ENGRAM_EVAL_CDP_PORT": str(cdp_port),
                "ENGRAM_EVAL_DRIVER_CONFIG": str(driver_config),
                "ENGRAM_EVAL_SESSION_ID": episode_id,
                "PLAYWRIGHT_MCP_CONFIG": str(config),
            }
        )
        for key in (
            "ENGRAM_EVAL_AGENT_BROWSER_COMMAND",
            "ENGRAM_EVAL_PLAYWRIGHT_COMMAND",
            "ENGRAM_EVAL_PLAYWRIGHT_013_COMMAND",
            "ENGRAM_EVAL_PLAYWRIGHT_017_COMMAND",
        ):
            env.pop(key, None)
        prompt = (
            "Complete the MiniWoB++ task already open in the shared browser. "
            f"The BrowserGym-generated goal is: {setup['goal']}\n"
            "Verify completion. Do not share any screenshot or recording."
        )
        agent_log_path = out / "agent.jsonl"
        with agent_log_path.open("wb") as agent_log:
            try:
                result = subprocess.run(
                    harness_command(
                        episode.harness,
                        workspace,
                        prompt,
                        mcp_config,
                        visual,
                        out,
                    ),
                    cwd=workspace,
                    env=env,
                    stdout=agent_log,
                    stderr=subprocess.STDOUT,
                    timeout=180,
                    check=False,
                )
                exit_code = result.returncode
                timed_out = False
            except subprocess.TimeoutExpired:
                exit_code = 124
                timed_out = True
        try:
            grade = driver_request(driver, {"command": "validate"})
        except (OSError, RuntimeError, TimeoutError, json.JSONDecodeError) as error:
            grade = {
                "type": "grade",
                "success": False,
                "reward": 0,
                "done": False,
                "message": f"BrowserGym validation failed: {error}",
            }
        selected_browser_command = (
            playwright_command if browser_tool == "playwright-cli" else agent_command
        )
        violated_protocol = protocol_violation(agent_log_path, selected_browser_command, base_url)
        recorded = read_events(events)
        shares = count_shared_artifacts(recorded)
        actions = int(action_count.read_text() if action_count.exists() else "0")
        max_repeat = no_progress_repeats(recorded)
        bounded = not timed_out and not violated_protocol and 0 < actions <= 30 and max_repeat <= 2
        return {
            **asdict(episode),
            "id": episode_id,
            "success": bool(grade["success"]) and shares == 0 and bounded,
            "taskSuccess": bool(grade["success"]),
            "shares": shares,
            "actions": actions,
            "maxIdenticalRepeat": max_repeat,
            "exitCode": exit_code,
            "timedOut": timed_out,
            "protocolViolation": violated_protocol,
            "durationMs": round((time.monotonic() - started) * 1000),
            "harnessVersion": command_version(episode.harness),
            "resolvedModel": resolved_model(agent_log_path)
            if episode.harness == "claude"
            else expected_model(episode.harness),
            "browserVersion": "playwright-cli@0.1.17"
            if episode.arm == "pw-latest"
            else "agent-browser@0.32.0",
            "goal": setup["goal"],
            "grade": grade,
        }
    finally:
        if driver is not None and driver.poll() is None:
            try:
                driver_request(driver, {"command": "close"}, timeout=2)
            except (OSError, RuntimeError, TimeoutError, json.JSONDecodeError):
                driver.kill()
            try:
                driver.wait(timeout=5)
            except subprocess.TimeoutExpired:
                driver.kill()
        if driver_log is not None:
            driver_log.close()
        chrome.send_signal(signal.SIGTERM)
        try:
            chrome.wait(timeout=5)
        except subprocess.TimeoutExpired:
            chrome.kill()
        chrome_log.close()
        shutil.rmtree(workspace, ignore_errors=True)


def run_miniwob_episode_safe(episode: MiniwobEpisode, out_root: Path) -> dict[str, object]:
    started = time.monotonic()
    try:
        return run_miniwob_episode(episode, out_root)
    except Exception as error:
        return {
            **asdict(episode),
            "id": f"miniwob-{episode.arm}-{episode.harness}-{episode.task}-s{episode.seed}",
            "success": False,
            "taskSuccess": False,
            "shares": 0,
            "actions": 0,
            "maxIdenticalRepeat": 0,
            "exitCode": 1,
            "timedOut": isinstance(error, TimeoutError),
            "protocolViolation": False,
            "durationMs": round((time.monotonic() - started) * 1000),
            "harnessVersion": command_version(episode.harness),
            "resolvedModel": expected_model(episode.harness),
            "browserVersion": (
                "playwright-cli@0.1.17"
                if episode.arm == "pw-latest"
                else "agent-browser@0.32.0"
            ),
            "goal": None,
            "grade": {
                "type": "grade",
                "success": False,
                "message": f"evaluation infrastructure failed: {error}",
            },
        }


def miniwob_gate(results: list[dict[str, object]], arms: list[str]) -> dict[str, object]:
    stats: dict[str, dict[str, object]] = {}
    for arm in arms:
        selected = [result for result in results if result["arm"] == arm]
        passed = sum(bool(result["success"]) for result in selected)
        stats[arm] = {
            "passes": passed,
            "failures": len(selected) - passed,
            "medianSuccessfulCommands": median(
                [int(result["actions"]) for result in selected if result["success"]]
            ),
        }
    best_failures = min(int(stat["failures"]) for stat in stats.values())
    medians = [
        float(stat["medianSuccessfulCommands"])
        for stat in stats.values()
        if stat["medianSuccessfulCommands"] is not None
    ]
    best_median = min(medians) if medians else None
    hybrid = stats.get("hybrid")
    passed = bool(
        hybrid
        and int(hybrid["failures"]) <= best_failures + 1
        and hybrid["medianSuccessfulCommands"] is not None
        and best_median is not None
        and float(hybrid["medianSuccessfulCommands"]) <= best_median * 1.10
    )
    return {
        "passed": passed,
        "arms": stats,
        "bestFailures": best_failures,
        "bestMedian": best_median,
    }


def write_miniwob_report(results: list[dict[str, object]], arms: list[str], out: Path) -> None:
    (out / "miniwob-results.json").write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
    (out / "miniwob-summary.json").write_text(
        json.dumps(
            {
                "benchmark": {
                    "browsergym": "browsergym-miniwob==0.14.3",
                    "miniwobRevision": "7fd85d71a4b60325c6585396ec4f48377d049838",
                },
                "gate": miniwob_gate(results, arms),
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )


def smoke() -> int:
    server, thread = start_server()
    try:
        for seed, cfg in enumerate(SEEDS):
            post_json(f"{server.url}/api/reset", {"seed": seed})
            assert get_json(f"{server.url}/api/config")["incident"] == cfg["incident"]
            initial = get_json(f"{server.url}/api/grade")
            assert initial["success"] is False
            post_json(f"{server.url}/api/select", {"incident": cfg["incident"]})
            submitted = post_json(
                f"{server.url}/api/submit", {"service": cfg["source"], "policy": cfg["policy"]}
            )
            assert submitted["ok"] is True
            assert get_json(f"{server.url}/api/grade")["success"] is True
        print(f"browser-eval smoke: {len(SEEDS)} deterministic seeds passed")
        return 0
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("smoke")
    matrix_parser = sub.add_parser("matrix")
    matrix_parser.add_argument("--dry-run", action="store_true")
    matrix_parser.add_argument("--arm", choices=ARMS, action="append")
    matrix_parser.add_argument("--harness", choices=HARNESSES, action="append")
    matrix_parser.add_argument("--seed", type=int, choices=range(len(SEEDS)), action="append")
    matrix_parser.add_argument("--evidence", choices=("plain", "requested", "both"), default="both")
    matrix_parser.add_argument("--jobs", type=int, choices=range(1, 5), default=1)
    miniwob_parser = sub.add_parser("miniwob")
    miniwob_parser.add_argument(
        "--arm",
        choices=("pw-latest", "agent-dom", "hybrid"),
        action="append",
        required=True,
    )
    miniwob_parser.add_argument("--harness", choices=HARNESSES, action="append")
    miniwob_parser.add_argument("--task", choices=MINIWOB_TASKS, action="append")
    miniwob_parser.add_argument("--seed", type=int, default=0)
    miniwob_parser.add_argument("--jobs", type=int, choices=range(1, 5), default=1)
    args = parser.parse_args()
    if args.command == "smoke":
        return smoke()
    if args.command == "miniwob":
        arms = list(dict.fromkeys(args.arm))
        harnesses = args.harness or list(HARNESSES)
        tasks = args.task or list(MINIWOB_TASKS)
        selected_miniwob = [
            MiniwobEpisode(arm, harness, task, args.seed)
            for arm in arms
            for harness in harnesses
            for task in tasks
        ]
        run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        out = ROOT / "artifacts" / "browser-eval" / f"{run_id}-miniwob"
        out.mkdir(parents=True)
        miniwob_results: list[dict[str, object]] = []
        if args.jobs == 1:
            for index, episode in enumerate(selected_miniwob, 1):
                print(f"[{index}/{len(selected_miniwob)}] {episode}", flush=True)
                miniwob_results.append(run_miniwob_episode_safe(episode, out))
                write_miniwob_report(miniwob_results, arms, out)
        else:
            print(
                f"running {len(selected_miniwob)} MiniWoB episodes with {args.jobs} isolated workers",
                flush=True,
            )
            with ThreadPoolExecutor(max_workers=args.jobs) as executor:
                futures = {
                    executor.submit(run_miniwob_episode_safe, episode, out): episode
                    for episode in selected_miniwob
                }
                for index, future in enumerate(as_completed(futures), 1):
                    result = future.result()
                    miniwob_results.append(result)
                    miniwob_results.sort(key=miniwob_key)
                    print(
                        f"[{index}/{len(selected_miniwob)}] {result['id']}: "
                        f"{'PASS' if result['success'] else 'FAIL'} ({result['durationMs']} ms)",
                        flush=True,
                    )
                    write_miniwob_report(miniwob_results, arms, out)
        print(out / "miniwob-summary.json")
        return 0 if all(bool(result["success"]) for result in miniwob_results) else 1
    selected = [
        e
        for e in matrix()
        if (not args.arm or e.arm in args.arm)
        and (not args.harness or e.harness in args.harness)
        and (not args.seed or e.seed in args.seed)
        and (args.evidence == "both" or e.evidence == (args.evidence == "requested"))
    ]
    if args.dry_run:
        print(json.dumps([asdict(e) for e in selected], indent=2))
        return 0
    run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = ROOT / "artifacts" / "browser-eval" / run_id
    out.mkdir(parents=True)
    results: list[dict[str, object]] = []
    if args.jobs == 1:
        for index, episode in enumerate(selected, 1):
            print(f"[{index}/{len(selected)}] {episode}", flush=True)
            result = run_episode(episode, out)
            results.append(result)
            write_report(results, out)
    else:
        print(f"running {len(selected)} episodes with {args.jobs} isolated workers", flush=True)
        with ThreadPoolExecutor(max_workers=args.jobs) as executor:
            futures = {executor.submit(run_episode, episode, out): episode for episode in selected}
            for index, future in enumerate(as_completed(futures), 1):
                result = future.result()
                results.append(result)
                results.sort(key=episode_key)
                print(
                    f"[{index}/{len(selected)}] {result['id']}: "
                    f"{'PASS' if result['success'] else 'FAIL'} ({result['durationMs']} ms)",
                    flush=True,
                )
                write_report(results, out)
    print(out / "report.html")
    return 0 if all(bool(r["success"]) for r in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
