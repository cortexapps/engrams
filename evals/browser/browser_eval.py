#!/usr/bin/env python3
"""Black-box browser capability evaluator for ADR 0097."""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import html
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import Iterable
from urllib.request import Request, urlopen

from incident_console import SEEDS, start_server, task_prompt
from image_mcp import handle as handle_image_request
from skill_matrix import (
    CLIS,
    CLI_SPECS,
    POLICIES,
    TASK_MODES,
    VISIONS,
    config_id,
    render_skill,
    skill_hash,
)


ROOT = Path(__file__).resolve().parents[2]
PROXY = Path(__file__).with_name("tool_proxy.py")
IMAGE_MCP = Path(__file__).with_name("image_mcp.py")
TOOL_ROOT = ROOT / "artifacts" / "browser-eval" / ".tools"
HARNESSES = ("codex", "claude")
CODEX_MODEL = os.environ.get("ENGRAM_EVAL_CODEX_MODEL", "gpt-5.4")
CLAUDE_MODEL = os.environ.get("ENGRAM_EVAL_CLAUDE_MODEL", "sonnet")


@dataclass(frozen=True)
class Episode:
    cli: str
    policy: str
    vision: str
    task_mode: str
    harness: str
    seed: int
    evidence: bool


def matrix() -> Iterable[Episode]:
    for cli in CLIS:
        for policy in POLICIES:
            for vision in VISIONS:
                for task_mode in TASK_MODES:
                    for harness in HARNESSES:
                        for seed in range(len(SEEDS)):
                            for evidence in (False, True):
                                yield Episode(
                                    cli, policy, vision, task_mode, harness, seed, evidence
                                )


def episode_key(result: dict[str, object]) -> tuple[int, int, int, int, int, int, bool]:
    return (
        CLIS.index(str(result["cli"])),
        POLICIES.index(str(result["policy"])),
        VISIONS.index(str(result["vision"])),
        TASK_MODES.index(str(result["task_mode"])),
        HARNESSES.index(str(result["harness"])),
        int(result["seed"]),
        bool(result["evidence"]),
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
    command = [
        chrome,
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        f"--remote-debugging-port={cdp_port}",
        "--remote-allow-origins=*",
        f"--user-data-dir={profile}",
        "--window-position=0,0",
        "--window-size=1440,1080",
        "about:blank",
    ]
    if os.environ.get("ENGRAM_EVAL_HEADFUL") != "1":
        command.insert(1, "--headless=new")
    return command


def free_port() -> int:
    import socket

    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def png_dimensions(path: Path) -> tuple[int, int]:
    header = path.read_bytes()[:24]
    if len(header) != 24 or not header.startswith(b"\x89PNG\r\n\x1a\n"):
        raise RuntimeError(f"{path} is not a PNG screenshot")
    return int.from_bytes(header[16:20], "big"), int.from_bytes(header[20:24], "big")


def wait_cdp(port: int) -> None:
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            get_json(f"http://127.0.0.1:{port}/json/version")
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"Chrome CDP :{port} did not become ready")


def write_proxies(bin_dir: Path, cli: str) -> None:
    bin_dir.mkdir(parents=True)
    browser_tool = CLI_SPECS[cli].tool
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
        "--setting-sources",
        "project,local",
    ]
    command += ["--mcp-config", str(mcp_config), "--strict-mcp-config"]
    return [*command, prompt]


def command_for_cli(cli: str) -> str:
    spec = CLI_SPECS[cli]
    env_key = {
        "pw013": "ENGRAM_EVAL_PLAYWRIGHT_013_COMMAND",
        "pw017": "ENGRAM_EVAL_PLAYWRIGHT_017_COMMAND",
        "agent032": "ENGRAM_EVAL_AGENT_BROWSER_COMMAND",
    }[cli]
    override = os.environ.get(env_key)
    if override:
        return override
    executable = prepared_executable(cli)
    if not executable.is_file():
        raise RuntimeError(
            f"{cli} is not prepared at {executable}; run "
            f"`python3 evals/browser/browser_eval.py prepare --cli {cli}`"
        )
    return str(executable)


def prepared_executable(cli: str) -> Path:
    if cli != "agent032":
        return TOOL_ROOT / cli / "node_modules" / ".bin" / CLI_SPECS[cli].executable
    system = {"Darwin": "darwin", "Linux": "linux"}.get(platform.system())
    machine = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "x64"}.get(
        platform.machine()
    )
    if not system or not machine:
        raise RuntimeError(
            f"agent-browser has no measured native binary for "
            f"{platform.system()} {platform.machine()}"
        )
    if system == "linux" and platform.libc_ver()[0].lower() == "musl":
        system = "linux-musl"
    return (
        TOOL_ROOT
        / cli
        / "node_modules"
        / "agent-browser"
        / "bin"
        / f"agent-browser-{system}-{machine}"
    )


def prepare_tools(clis: list[str]) -> None:
    for cli in clis:
        spec = CLI_SPECS[cli]
        destination = TOOL_ROOT / cli
        executable = prepared_executable(cli)
        package_json = (
            destination / "node_modules" / "agent-browser" / "package.json"
            if cli == "agent032"
            else destination / "node_modules" / "@playwright" / "cli" / "package.json"
        )
        if executable.is_file() and package_json.is_file():
            installed = json.loads(package_json.read_text(encoding="utf-8"))
            if installed.get("version") == spec.version:
                print(f"{cli}: prepared {executable}")
                continue
        destination.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            [
                "npm",
                "install",
                "--prefix",
                str(destination),
                "--no-save",
                "--no-audit",
                "--no-fund",
                "--ignore-scripts",
                spec.package,
            ],
            check=True,
        )
        if not executable.is_file():
            raise RuntimeError(f"npm did not install {spec.executable} at {executable}")
        print(f"{cli}: prepared {executable}")


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


def install_candidate_skills(workspace: Path, instructions: str) -> Path:
    """Mirror engrams' harness-agnostic skill discovery layout."""
    skill_dir = workspace / ".agents" / "skills" / "browser"
    skill_dir.mkdir(parents=True)
    skill_path = skill_dir / "SKILL.md"
    skill_path.write_text(instructions, encoding="utf-8")
    share_dir = workspace / ".agents" / "skills" / "share-file"
    share_dir.mkdir()
    shutil.copyfile(
        ROOT / "deploy" / "bundles" / "skills" / "skills" / "share-file" / "SKILL.md",
        share_dir / "SKILL.md",
    )
    claude_root = workspace / ".claude"
    claude_root.mkdir()
    (claude_root / "skills").symlink_to(Path("..") / ".agents" / "skills")
    return skill_path


def isolate_harness_home(
    harness: str, workspace: Path, env: dict[str, str]
) -> None:
    """Keep host skills and settings out while retaining Codex's login."""
    if harness == "claude":
        # Claude's local subscription login is brokered through its normal
        # config/daemon path. `--setting-sources project,local` excludes user
        # settings and plugins while keeping that authentication path intact.
        env.pop("CLAUDE_CONFIG_DIR", None)
        env.pop("CLAUDE_SECURESTORAGE_CONFIG_DIR", None)
        return
    home = workspace / ".home"
    codex_home = home / ".codex"
    codex_home.mkdir(parents=True)
    source_codex_home = Path(
        os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))
    )
    source_auth = source_codex_home / "auth.json"
    if source_auth.is_file():
        (codex_home / "auth.json").symlink_to(source_auth)
    env.update(
        {
            "HOME": str(home),
            "CODEX_HOME": str(codex_home),
        }
    )


def skill_loaded(log: Path, skill_path: Path) -> bool:
    if not log.exists():
        return False
    raw = log.read_text(encoding="utf-8", errors="replace")
    if str(skill_path) in raw or ("SKILL.md" in raw and "browser" in raw.lower()):
        return True
    for line in raw.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue

        def visit(item: object) -> bool:
            if isinstance(item, dict):
                name = str(item.get("name", item.get("tool_name", ""))).lower()
                if name == "skill" and "browser" in json.dumps(item).lower():
                    return True
                return any(visit(child) for child in item.values())
            if isinstance(item, list):
                return any(visit(child) for child in item)
            return False

        if visit(value):
            return True
    return False


def episode_prompt(task_url: str, seed: int, evidence: bool, task_mode: str) -> str:
    return f"Task URL: {task_url}\n\n{task_prompt(seed, evidence, mode=task_mode)}"


def normalized_skill_hash(
    instructions: str, task_url: str, observation_dir: Path, deliverable_dir: Path
) -> str:
    normalized = (
        instructions.replace(task_url, "TASK_URL")
        .replace(str(observation_dir), "OBSERVATION_DIR")
        .replace(str(deliverable_dir), "DELIVERABLE_DIR")
    )
    return skill_hash(normalized)


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
                if item.get("type") == "tool_use" and item.get("name") == "Bash":
                    tool_input = item.get("input")
                    if isinstance(tool_input, dict) and isinstance(
                        tool_input.get("command"), str
                    ):
                        commands.append(str(tool_input["command"]))
                for child in item.values():
                    visit(child)
            elif isinstance(item, list):
                for child in item:
                    visit(child)

        visit(value)
    return commands


def protocol_violation(
    log: Path,
    raw_browser_command: str,
    task_url: str,
    observation_root: Path | None = None,
) -> bool:
    raw_executable = shlex.split(raw_browser_command)[0]
    task_host = task_url.removeprefix("http://").split("/", 1)[0]
    for command in executed_shell_commands(log):
        lowered = command.lower()
        if raw_executable in command or "engram_eval_" in lowered:
            return True
        if any(
            private_name in lowered
            for private_name in (
                "driver.json",
                "events.jsonl",
                "action-count",
                "pending-view",
                "evals/browser",
            )
        ):
            return True
        if task_host in command and any(
            marker in lowered
            for marker in ("curl ", "wget ", "urlopen", "requests.get", "httpx.")
        ):
            return True
        if "/api/" in lowered:
            return True
        if observation_root and str(observation_root) in command and any(
            marker in lowered for marker in ("cat ", "sed ", "base64 ", "open ")
        ):
            return True
    if observation_root and log.exists():
        observation = str(observation_root)
        for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue

            def has_direct_image_read(item: object) -> bool:
                if isinstance(item, dict):
                    if item.get("type") == "tool_use" and str(
                        item.get("name", "")
                    ).lower() in {"read", "view_image"}:
                        return observation in json.dumps(item)
                    return any(has_direct_image_read(child) for child in item.values())
                if isinstance(item, list):
                    return any(has_direct_image_read(child) for child in item)
                return False

            if has_direct_image_read(value):
                return True
    return False


def harness_tool_calls(log: Path) -> int:
    if not log.exists():
        return 0
    count = 0
    for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue

        def visit(item: object) -> None:
            nonlocal count
            if isinstance(item, dict):
                if item.get("type") == "command_execution" and item.get(
                    "status"
                ) == "completed":
                    count += 1
                elif item.get("type") == "tool_use":
                    count += 1
                for child in item.values():
                    visit(child)
            elif isinstance(item, list):
                for child in item:
                    visit(child)

        visit(value)
    return count


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


def count_valid_shared_artifacts(events: list[dict[str, object]]) -> int:
    return sum(
        1
        for event in events
        if event.get("type") == "share" and bool(event.get("valid"))
    )


def count_screenshots(events: list[dict[str, object]]) -> int:
    """Count attempted screenshots, excluding capability-discovery probes."""
    probes = {"-h", "--help", "-V", "--version"}
    return sum(
        1
        for event in events
        if event.get("type") == "browser_start"
        and [str(arg) for arg in event.get("argv", [])][:1] == ["screenshot"]
        and not probes.intersection(str(arg) for arg in event.get("argv", []))
    )


def count_screenshot_roles(events: list[dict[str, object]]) -> dict[str, int]:
    roles = {"internal": 0, "deliverable": 0, "unclassified": 0}
    probes = {"-h", "--help", "-V", "--version"}
    for event in events:
        argv = [str(arg) for arg in event.get("argv", [])]
        if (
            event.get("type") != "browser_start"
            or argv[:1] != ["screenshot"]
            or probes.intersection(argv)
        ):
            continue
        role = str(event.get("screenshotRole") or "unclassified")
        roles[role if role in roles else "unclassified"] += 1
    return roles


MUTATING_BROWSER_COMMANDS = {
    "open",
    "goto",
    "click",
    "dblclick",
    "fill",
    "type",
    "select",
    "check",
    "uncheck",
    "press",
    "hover",
    "drag",
    "upload",
}


def max_identical_failed_attempts(events: list[dict[str, object]]) -> int:
    """Return the longest run of identical failed mutation attempts.

    Observations are deliberately excluded: taking two fresh snapshots while
    verifying a stable page is not a failed action. A mutation is failed only
    when the sanitized page state is unchanged across that command.
    """
    starts = [e for e in events if e.get("type") == "browser_start"]
    ends = {e.get("action"): e for e in events if e.get("type") == "browser_end"}
    longest = 0
    previous: tuple[object, object, object] | None = None
    run = 0
    for event in starts:
        argv = [str(arg) for arg in event.get("argv", [])]
        if not argv or argv[0] not in MUTATING_BROWSER_COMMANDS:
            previous = None
            run = 0
            continue
        end = ends.get(event.get("action"), {})
        before = event.get("before", {})
        after = end.get("after", {}) if isinstance(end, dict) else {}
        before_hash = before.get("stateHash") if isinstance(before, dict) else None
        state_hash = after.get("stateHash") if isinstance(after, dict) else None
        if before_hash is None or state_hash is None or before_hash != state_hash:
            previous = None
            run = 0
            continue
        current = (event.get("tool"), argv, before_hash)
        if current == previous:
            run += 1
        else:
            run = 1
        longest = max(longest, run)
        previous = current
    previous_gate: tuple[object, object, object] | None = None
    gate_run = 0
    for event in events:
        if event.get("type") != "visual_gate":
            continue
        current_gate = (
            event.get("tool"),
            event.get("argv"),
            event.get("requiredPath"),
        )
        gate_run = gate_run + 1 if current_gate == previous_gate else 1
        longest = max(longest, gate_run)
        previous_gate = current_gate
    return longest


def run_episode(episode: Episode, out_root: Path) -> dict[str, object]:
    started = time.monotonic()
    wall_clock_limit_seconds = 180
    configuration = config_id(episode.cli, episode.policy, episode.vision)
    episode_id = (
        f"{configuration}-{episode.task_mode}-{episode.harness}-s{episode.seed}-"
        f"{'evidence' if episode.evidence else 'plain'}"
    )
    session_name = "be-" + hashlib.sha256(episode_id.encode()).hexdigest()[:12]
    out = out_root / episode_id
    out.mkdir(parents=True, exist_ok=True)
    workspace = Path(tempfile.mkdtemp(prefix=f"engrams-{episode_id}-"))
    agent_socket_dir = Path(tempfile.mkdtemp(prefix="abe-", dir="/tmp"))
    path_alias = Path(tempfile.mkdtemp(prefix="be-ep-", dir="/tmp"))
    path_alias.rmdir()
    path_alias.symlink_to(out, target_is_directory=True)
    server, thread = start_server()
    post_json(
        f"{server.url}/api/reset", {"seed": episode.seed, "mode": episode.task_mode}
    )
    cdp_port = free_port()
    profile = out / "chrome-profile"
    chrome_log = (out / "chrome.log").open("wb")
    chrome = subprocess.Popen(
        chrome_command(profile, cdp_port), stdout=chrome_log, stderr=subprocess.STDOUT
    )
    try:
        wait_cdp(cdp_port)
        observation_dir = path_alias / "observations"
        observation_dir.mkdir()
        deliverable_dir = path_alias / "deliverables"
        deliverable_dir.mkdir()
        instructions = render_skill(
            episode.cli,
            episode.policy,
            episode.vision,
            server.url,
            str(observation_dir),
            str(deliverable_dir),
        )
        (out / "SKILL.md").write_text(instructions, encoding="utf-8")
        skill_path = install_candidate_skills(workspace, instructions)
        bin_dir = out / "bin"
        write_proxies(bin_dir, episode.cli)
        events = out / "events.jsonl"
        action_count = out / "action-count"
        pending_view = out / "pending-view"
        browser_command = command_for_cli(episode.cli)
        browser_tool = CLI_SPECS[episode.cli].tool
        driver_config = out / "driver.json"
        driver_config.write_text(
            json.dumps(
                {
                    "cdpPort": cdp_port,
                    "sessionName": session_name,
                    "commands": {browser_tool: browser_command},
                }
            ),
            encoding="utf-8",
        )
        config = out / "playwright.config.json"
        config.write_text(
            json.dumps({"browser": {"cdpEndpoint": f"http://127.0.0.1:{cdp_port}"}})
        )
        mcp_config = out / "mcp.json"
        visual = episode.vision == "on"
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
        isolate_harness_home(episode.harness, workspace, env)
        env.update(
            {
                "PATH": f"{bin_dir}{os.pathsep}{env['PATH']}",
                "ENGRAM_EVAL_EVENTS": str(events),
                "ENGRAM_EVAL_ACTION_COUNT": str(action_count),
                "ENGRAM_EVAL_ACTION_LIMIT": "30",
                "ENGRAM_EVAL_VISUAL_GATE": "1" if visual else "0",
                "ENGRAM_EVAL_PENDING_VIEW": str(pending_view),
                "ENGRAM_EVAL_OBSERVATION_ROOT": str(observation_dir),
                "ENGRAM_EVAL_DELIVERABLE_ROOT": str(deliverable_dir),
                "ENGRAM_EVAL_EVIDENCE_REQUESTED": "1" if episode.evidence else "0",
                "ENGRAM_EVAL_CDP_PORT": str(cdp_port),
                "ENGRAM_EVAL_STATE_URL": f"{server.url}/api/observable",
                "ENGRAM_EVAL_GRADE_URL": f"{server.url}/api/grade",
                "ENGRAM_EVAL_DRIVER_CONFIG": str(driver_config),
                "ENGRAM_EVAL_SESSION_ID": episode_id,
                "AGENT_BROWSER_SOCKET_DIR": str(agent_socket_dir),
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
        prompt = episode_prompt(
            server.url, episode.seed, episode.evidence, episode.task_mode
        )
        agent_log_path = out / "agent.jsonl"
        with agent_log_path.open("wb") as agent_log:
            try:
                remaining = wall_clock_limit_seconds - (time.monotonic() - started)
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(
                        harness_command(
                            episode.harness,
                            workspace,
                            prompt,
                            mcp_config,
                            visual,
                            out,
                        ),
                        wall_clock_limit_seconds,
                    )
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
                    timeout=remaining,
                    check=False,
                )
                exit_code = result.returncode
                timed_out = False
            except subprocess.TimeoutExpired:
                exit_code = 124
                timed_out = True
        grade = get_json(f"{server.url}/api/grade")
        violated_protocol = protocol_violation(
            agent_log_path, browser_command, server.url, observation_dir
        )
        recorded = read_events(events)
        shares = count_shared_artifacts(recorded)
        valid_shares = count_valid_shared_artifacts(recorded)
        screenshots = count_screenshots(recorded)
        screenshot_roles = count_screenshot_roles(recorded)
        expected_shares = 1 if episode.evidence and grade["success"] else 0
        share_intent_success = shares == expected_shares and valid_shares == expected_shares
        actions = int(action_count.read_text() if action_count.exists() else "0")
        max_failed_attempts = max_identical_failed_attempts(recorded)
        loaded_skill = skill_loaded(agent_log_path, skill_path)
        screenshot_policy_success = not any(
            event.get("type") == "screenshot_policy" for event in recorded
        )
        expected_internal_screenshots = (
            1
            if visual and episode.task_mode == "visual" and bool(grade["success"])
            else 0
        )
        expected_deliverable_screenshots = expected_shares
        visual_intent_success = (
            screenshot_roles["internal"] == expected_internal_screenshots
            and screenshot_roles["deliverable"] == expected_deliverable_screenshots
            and screenshot_roles["unclassified"] == 0
            and not pending_view.exists()
        )
        visual_gate_blocks = sum(
            event.get("type") == "visual_gate" for event in recorded
        )
        duration_ms = round((time.monotonic() - started) * 1000)
        bounded = (
            not timed_out
            and not violated_protocol
            and 0 < actions <= 30
            and max_failed_attempts <= 2
            and duration_ms <= wall_clock_limit_seconds * 1000
        )
        return {
            **asdict(episode),
            "id": episode_id,
            "configuration": configuration,
            "success": (
                bool(grade["success"])
                and share_intent_success
                and bounded
                and loaded_skill
                and screenshot_policy_success
                and visual_intent_success
            ),
            "taskSuccess": bool(grade["success"]),
            "shareIntentSuccess": share_intent_success,
            "shares": shares,
            "validShares": valid_shares,
            "screenshots": screenshots,
            "internalScreenshots": screenshot_roles["internal"],
            "deliverableScreenshots": screenshot_roles["deliverable"],
            "screenshotPolicySuccess": screenshot_policy_success,
            "visualIntentSuccess": visual_intent_success,
            "visualGateBlocks": visual_gate_blocks,
            "actions": actions,
            "harnessToolCalls": harness_tool_calls(agent_log_path),
            "maxIdenticalFailedAttempts": max_failed_attempts,
            "exitCode": exit_code,
            "timedOut": timed_out,
            "protocolViolation": violated_protocol,
            "durationMs": duration_ms,
            "skillHash": skill_hash(instructions),
            "skillTemplateHash": normalized_skill_hash(
                instructions, server.url, observation_dir, deliverable_dir
            ),
            "skillLoaded": loaded_skill,
            "sessionName": session_name,
            "chromeMode": (
                "headful" if os.environ.get("ENGRAM_EVAL_HEADFUL") == "1" else "headless-new"
            ),
            "windowGeometry": "1440x1080",
            "emulationTier": (
                "production-geometry-headful"
                if os.environ.get("ENGRAM_EVAL_HEADFUL") == "1"
                else "diagnostic-headless"
            ),
            "harnessVersion": command_version(episode.harness),
            "resolvedModel": (
                resolved_model(agent_log_path)
                if episode.harness == "claude"
                else expected_model(episode.harness)
            ),
            "browserVersion": f"{browser_tool}@{CLI_SPECS[episode.cli].version}",
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
        shutil.rmtree(agent_socket_dir, ignore_errors=True)
        path_alias.unlink(missing_ok=True)


def median(values: list[int]) -> int | float | None:
    values = sorted(values)
    if not values:
        return None
    middle = len(values) // 2
    return values[middle] if len(values) % 2 else (values[middle - 1] + values[middle]) / 2


def configuration_summary(results: list[dict[str, object]]) -> dict[str, dict[str, object]]:
    summary: dict[str, dict[str, object]] = {}
    for configuration in sorted({str(result["configuration"]) for result in results}):
        selected = [
            result for result in results if result["configuration"] == configuration
        ]
        summary[configuration] = {
            "episodes": len(selected),
            "passes": sum(bool(result["success"]) for result in selected),
            "taskPasses": sum(bool(result["taskSuccess"]) for result in selected),
            "skillLoads": sum(bool(result["skillLoaded"]) for result in selected),
            "screenshotPolicyPasses": sum(
                bool(result["screenshotPolicySuccess"]) for result in selected
            ),
            "visualIntentPasses": sum(
                bool(result["visualIntentSuccess"]) for result in selected
            ),
            "visualGateBlocks": sum(
                int(result["visualGateBlocks"]) for result in selected
            ),
            "sharePasses": sum(bool(result["shareIntentSuccess"]) for result in selected),
            "timeouts": sum(bool(result["timedOut"]) for result in selected),
            "maxIdenticalFailedAttempts": max(
                int(result["maxIdenticalFailedAttempts"]) for result in selected
            ),
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
            "byTaskMode": {
                mode: sum(
                    bool(result["success"])
                    for result in selected
                    if result["task_mode"] == mode
                )
                for mode in TASK_MODES
            },
        }
    return summary


def incident_gate(results: list[dict[str, object]]) -> dict[str, object]:
    if not results:
        return {"evaluated": False, "reason": "no episodes"}
    configurations: dict[str, dict[str, bool]] = {}
    for configuration in sorted({str(result["configuration"]) for result in results}):
        selected = [
            result for result in results if result["configuration"] == configuration
        ]
        configurations[configuration] = {
            "allWithinBounds": all(
                not bool(result["timedOut"])
                and int(result.get("durationMs", 0)) <= 180_000
                and 0 < int(result["actions"]) <= 30
                for result in selected
            ),
            "noProtocolViolations": all(
                not bool(result.get("protocolViolation")) for result in selected
            ),
            "skillDiscovered": all(bool(result.get("skillLoaded")) for result in selected),
            "screenshotPolicyExact": all(
                bool(result.get("screenshotPolicySuccess")) for result in selected
            ),
            "visualIntentExact": all(
                bool(result.get("visualIntentSuccess")) for result in selected
            ),
            "noMoreThanTwoIdenticalFailures": all(
                int(result["maxIdenticalFailedAttempts"]) <= 2 for result in selected
            ),
            "sharingExact": all(bool(result["shareIntentSuccess"]) for result in selected),
        }
    return {
        "evaluated": True,
        "passed": all(all(checks.values()) for checks in configurations.values()),
        "configurations": configurations,
        "selection": "none; a factorial sweep diagnoses factors but does not select an actuator",
    }


def write_report(results: list[dict[str, object]], out: Path) -> None:
    # Re-grade sharing from the append-only traces. This intentionally makes
    # report generation robust to an older logging shim that classified an
    # `engram-share --help` capability probe as a delivery attempt.
    for result in results:
        events = out / str(result["id"]) / "events.jsonl"
        if events.exists():
            recorded = read_events(events)
            shares = count_shared_artifacts(recorded)
            valid_shares = count_valid_shared_artifacts(recorded)
            result["shares"] = shares
            result["validShares"] = valid_shares
            result["screenshots"] = count_screenshots(recorded)
            screenshot_roles = count_screenshot_roles(recorded)
            result["internalScreenshots"] = screenshot_roles["internal"]
            result["deliverableScreenshots"] = screenshot_roles["deliverable"]
            result["maxIdenticalFailedAttempts"] = max_identical_failed_attempts(
                recorded
            )
            result["visualGateBlocks"] = sum(
                event.get("type") == "visual_gate" for event in recorded
            )
            result["screenshotPolicySuccess"] = not any(
                event.get("type") == "screenshot_policy" for event in recorded
            )
            expected_shares = (
                1 if bool(result.get("evidence")) and bool(result["taskSuccess"]) else 0
            )
            result["shareIntentSuccess"] = (
                shares == expected_shares and valid_shares == expected_shares
            )
            expected_internal = (
                1
                if result.get("vision") == "on"
                and result.get("task_mode") == "visual"
                and bool(result["taskSuccess"])
                else 0
            )
            result["visualIntentSuccess"] = (
                screenshot_roles["internal"] == expected_internal
                and screenshot_roles["deliverable"] == expected_shares
                and screenshot_roles["unclassified"] == 0
                and not (out / str(result["id"]) / "pending-view").exists()
            )
    # A timed-out or command-limit episode is a failed episode even if it
    # happened to mutate the task into the right state before being killed.
    for result in results:
        result["success"] = bool(result["taskSuccess"]) and bool(
            result["shareIntentSuccess"]
        ) and not bool(result["timedOut"]) and not bool(
            result.get("protocolViolation")
        ) and bool(result.get("skillLoaded")) and bool(
            result.get("screenshotPolicySuccess")
        ) and bool(
            result.get("visualIntentSuccess")
        ) and int(result.get("durationMs", 0)) <= 180_000 and 0 < int(
            result["actions"]
        ) <= 30 and int(result["maxIdenticalFailedAttempts"]) <= 2
    (out / "results.json").write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
    summary = {
        "generatedAt": datetime.now(timezone.utc).isoformat(),
        "models": {"codex": CODEX_MODEL, "claudeRequested": CLAUDE_MODEL},
        "configurations": configuration_summary(results),
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


def smoke() -> int:
    server, thread = start_server()
    try:
        for seed, cfg in enumerate(SEEDS):
            post_json(f"{server.url}/api/reset", {"seed": seed})
            assert get_json(f"{server.url}/api/config")["incident"] == cfg["incident"]
            initial = get_json(f"{server.url}/api/grade")
            assert initial["success"] is False
            post_json(
                f"{server.url}/api/observe",
                {"filter": cfg["incident"]},
            )
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


def preflight_cli(cli: str) -> None:
    """Exercise the real pinned CLI and symmetric visual gate without a model."""
    root = Path(tempfile.mkdtemp(prefix=f"browser-eval-preflight-{cli}-"))
    socket_dir = Path(tempfile.mkdtemp(prefix="abe-pre-", dir="/tmp"))
    server, thread = start_server()
    post_json(f"{server.url}/api/reset", {"seed": 0, "mode": "visual"})
    cdp_port = free_port()
    profile = root / "chrome-profile"
    chrome_log = (root / "chrome.log").open("wb")
    chrome = subprocess.Popen(
        chrome_command(profile, cdp_port), stdout=chrome_log, stderr=subprocess.STDOUT
    )
    try:
        wait_cdp(cdp_port)
        bin_dir = root / "bin"
        write_proxies(bin_dir, cli)
        observation_dir = root / "observations"
        observation_dir.mkdir()
        pending_view = root / "pending-view"
        events = root / "events.jsonl"
        action_count = root / "action-count"
        spec = CLI_SPECS[cli]
        browser_command = command_for_cli(cli)
        session_name = "be-preflight"
        driver_config = root / "driver.json"
        driver_config.write_text(
            json.dumps(
                {
                    "cdpPort": cdp_port,
                    "sessionName": session_name,
                    "commands": {spec.tool: browser_command},
                }
            ),
            encoding="utf-8",
        )
        playwright_config = root / "playwright.config.json"
        playwright_config.write_text(
            json.dumps({"browser": {"cdpEndpoint": f"http://127.0.0.1:{cdp_port}"}}),
            encoding="utf-8",
        )
        env = os.environ.copy()
        env.update(
            {
                "PATH": f"{bin_dir}{os.pathsep}{env['PATH']}",
                "ENGRAM_EVAL_EVENTS": str(events),
                "ENGRAM_EVAL_ACTION_COUNT": str(action_count),
                "ENGRAM_EVAL_ACTION_LIMIT": "10",
                "ENGRAM_EVAL_VISUAL_GATE": "1",
                "ENGRAM_EVAL_PENDING_VIEW": str(pending_view),
                "ENGRAM_EVAL_OBSERVATION_ROOT": str(observation_dir),
                "ENGRAM_EVAL_CDP_PORT": str(cdp_port),
                "ENGRAM_EVAL_STATE_URL": f"{server.url}/api/observable",
                "ENGRAM_EVAL_DRIVER_CONFIG": str(driver_config),
                "AGENT_BROWSER_SOCKET_DIR": str(socket_dir),
                "PLAYWRIGHT_MCP_CONFIG": str(playwright_config),
            }
        )

        def invoke(*args: str, expected: int = 0) -> subprocess.CompletedProcess[str]:
            result = subprocess.run(
                [spec.tool, *args],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
                timeout=40,
            )
            if result.returncode != expected:
                raise RuntimeError(
                    f"{spec.tool} {' '.join(args)} exited {result.returncode}: "
                    f"{result.stdout}{result.stderr}"
                )
            return result

        invoke("open", server.url)
        snapshot_args = (
            ("snapshot",) if spec.tool == "playwright-cli" else ("snapshot", "-i")
        )
        snapshot_result = invoke(*snapshot_args)
        if "acknowledge" not in snapshot_result.stdout.lower():
            raise RuntimeError(f"{cli} snapshot did not expose the shared page semantics")
        screenshot = observation_dir / f"{cli}.png"
        if spec.tool == "playwright-cli":
            invoke("screenshot", "--filename", str(screenshot))
        else:
            invoke("screenshot", "--annotate", str(screenshot))
        width, height = png_dimensions(screenshot)
        if width < 1200 or height < 900:
            raise RuntimeError(
                f"{cli} screenshot was {width}x{height}, below production geometry"
            )
        if pending_view.read_text(encoding="utf-8").strip() != str(screenshot.resolve()):
            raise RuntimeError(f"{cli} did not create the symmetric visual interlock")
        invoke(*snapshot_args, expected=125)
        response = handle_image_request(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "browser_view", "arguments": {"path": str(screenshot)}},
            },
            [root],
        )
        if response is None or response.get("result", {}).get("isError"):
            raise RuntimeError(f"browser_view rejected {cli} screenshot: {response}")
        if pending_view.exists():
            raise RuntimeError(f"browser_view did not clear {cli} visual interlock")
        post_view = invoke(*snapshot_args)
        ref_prefix = "" if spec.tool == "playwright-cli" else "@"

        def ref_for(output: str, label: str) -> str:
            match = re.search(
                rf'[^\n]*{re.escape(label)}[^\n]*\[ref=(e\d+)\]', output
            )
            if not match:
                raise RuntimeError(f"{cli} snapshot did not expose {label!r}")
            return ref_prefix + match.group(1)

        invoke("click", ref_for(post_view.stdout, "Acknowledge and continue"))
        after_ack = invoke(*snapshot_args)
        invoke("fill", ref_for(after_ack.stdout, "Filter incidents"), SEEDS[0]["incident"])
        observable = get_json(f"{server.url}/api/observable")
        if observable.get("filter") != SEEDS[0]["incident"]:
            raise RuntimeError(f"{cli} did not mutate the shared semantic filter state")
        print(
            f"{cli}: CLI/CDP/snapshot/filter/{width}x{height} screenshot/browser_view "
            "preflight passed"
        )
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
        shutil.rmtree(root, ignore_errors=True)
        shutil.rmtree(socket_dir, ignore_errors=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("smoke")
    prepare_parser = sub.add_parser("prepare")
    prepare_parser.add_argument("--cli", choices=CLIS, action="append")
    preflight_parser = sub.add_parser("preflight")
    preflight_parser.add_argument("--cli", choices=CLIS, action="append")
    matrix_parser = sub.add_parser("matrix")
    matrix_parser.add_argument("--dry-run", action="store_true")
    matrix_parser.add_argument("--cli", choices=CLIS, action="append")
    matrix_parser.add_argument("--policy", choices=POLICIES, action="append")
    matrix_parser.add_argument("--vision", choices=VISIONS, action="append")
    matrix_parser.add_argument("--task-mode", choices=TASK_MODES, action="append")
    matrix_parser.add_argument(
        "--matched-capability",
        action="store_true",
        help="pair semantic-only capability with DOM tasks and adaptive vision with visual tasks",
    )
    matrix_parser.add_argument("--harness", choices=HARNESSES, action="append")
    matrix_parser.add_argument("--seed", type=int, choices=range(len(SEEDS)), action="append")
    matrix_parser.add_argument("--evidence", choices=("plain", "requested", "both"), default="both")
    matrix_parser.add_argument("--jobs", type=int, choices=range(1, 5), default=1)
    matrix_parser.add_argument(
        "--headful",
        action="store_true",
        help="run local Chrome headfully at the production 1440x1080 geometry",
    )
    args = parser.parse_args()
    if args.command == "smoke":
        return smoke()
    if args.command == "prepare":
        prepare_tools(args.cli or list(CLIS))
        return 0
    if args.command == "preflight":
        for cli in args.cli or ["pw017", "agent032"]:
            preflight_cli(cli)
        return 0
    selected_clis = args.cli or ["pw017", "agent032"]
    selected_policies = args.policy or list(POLICIES)
    selected_visions = args.vision or list(VISIONS)
    selected_modes = args.task_mode or list(TASK_MODES)
    selected_harnesses = args.harness or list(HARNESSES)
    selected_seeds = args.seed or [0]
    selected = [
        e
        for e in matrix()
        if e.cli in selected_clis
        and e.policy in selected_policies
        and e.vision in selected_visions
        and e.task_mode in selected_modes
        and (
            not args.matched_capability
            or e.vision == ("on" if e.task_mode == "visual" else "off")
        )
        and e.harness in selected_harnesses
        and e.seed in selected_seeds
        and (args.evidence == "both" or e.evidence == (args.evidence == "requested"))
    ]
    if args.dry_run:
        print(json.dumps([asdict(e) for e in selected], indent=2))
        return 0
    if args.headful:
        os.environ["ENGRAM_EVAL_HEADFUL"] = "1"
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
