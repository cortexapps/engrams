from contextlib import redirect_stdout
from io import StringIO
import json
import os
from pathlib import Path
from threading import Thread
import tempfile
import time
import unittest
from urllib.request import Request, urlopen

from incident_console import SEEDS, start_server, task_prompt
from image_mcp import handle
from browser_eval import (
    chrome_command,
    count_screenshots,
    count_screenshot_roles,
    count_shared_artifacts,
    count_valid_shared_artifacts,
    episode_prompt,
    harness_command,
    incident_gate,
    install_candidate_skills,
    matrix,
    normalized_skill_hash,
    max_identical_failed_attempts,
    png_dimensions,
    protocol_violation,
    write_proxies,
)
from skill_matrix import CLIS, POLICIES, TASK_MODES, VISIONS, render_skill
from tool_proxy import (
    browser,
    internal_screenshot_path,
    introspection_block_reason,
    remove_isolation_overrides,
    share,
    screenshot_path,
    screenshot_policy_error,
)


def post(url: str, body: dict[str, object]) -> dict[str, object]:
    req = Request(
        url,
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urlopen(req) as response:
        return json.load(response)


def get(url: str) -> dict[str, object]:
    with urlopen(url) as response:
        return json.load(response)


class IncidentConsoleTest(unittest.TestCase):
    def setUp(self) -> None:
        self.server, self.thread = start_server()

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def test_each_seed_requires_exact_state(self) -> None:
        for seed, cfg in enumerate(SEEDS):
            with self.subTest(seed=seed):
                post(f"{self.server.url}/api/reset", {"seed": seed})
                post(
                    f"{self.server.url}/api/observe",
                    {"filter": cfg["incident"]},
                )
                post(f"{self.server.url}/api/select", {"incident": cfg["incident"]})
                result = post(
                    f"{self.server.url}/api/submit",
                    {"service": cfg["source"], "policy": cfg["policy"]},
                )
                self.assertTrue(result["ok"])
                self.assertTrue(get(f"{self.server.url}/api/grade")["success"])

    def test_wrong_submission_is_a_collateral_mutation(self) -> None:
        cfg = SEEDS[0]
        post(f"{self.server.url}/api/reset", {"seed": 0})
        post(f"{self.server.url}/api/select", {"incident": cfg["incident"]})
        result = post(
            f"{self.server.url}/api/submit",
            {"service": cfg["impacted"], "policy": cfg["policy"]},
        )
        self.assertFalse(result["ok"])
        self.assertEqual(get(f"{self.server.url}/api/grade")["wrongMutations"], 1)

    def test_prompt_variants_lock_sharing_intent(self) -> None:
        self.assertIn("Do not share", task_prompt(0, False))
        self.assertNotIn("internal", task_prompt(0, False).lower())
        self.assertIn("share exactly one", task_prompt(0, True))

    def test_private_image_tool_returns_mcp_image_content(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "page.png"
            path.write_bytes(b"\x89PNG\r\n\x1a\nabc")
            response = handle(
                {
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "tools/call",
                    "params": {"name": "browser_view", "arguments": {"path": str(path)}},
                },
                [Path(directory)],
            )
        self.assertEqual(response["result"]["content"][1]["type"], "image")
        self.assertEqual(response["result"]["content"][1]["mimeType"], "image/png")

    def test_png_dimensions_reads_the_screenshot_header(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "page.png"
            path.write_bytes(
                b"\x89PNG\r\n\x1a\n"
                + b"\x00\x00\x00\rIHDR"
                + (1440).to_bytes(4, "big")
                + (1080).to_bytes(4, "big")
            )
            self.assertEqual(png_dimensions(path), (1440, 1080))

    def test_factorial_gate_requires_every_configuration_to_obey_invariants(self) -> None:
        results = []
        for episode in matrix():
            results.append(
                {
                    "configuration": f"{episode.cli}-{episode.policy}-{episode.vision}",
                    "harness": episode.harness,
                    "task_mode": episode.task_mode,
                    "success": True,
                    "taskSuccess": True,
                    "skillLoaded": True,
                    "screenshotPolicySuccess": True,
                    "visualIntentSuccess": True,
                    "visualGateBlocks": 0,
                    "shareIntentSuccess": True,
                    "timedOut": False,
                    "protocolViolation": False,
                    "actions": 8,
                    "maxIdenticalFailedAttempts": 0,
                }
            )
        gate = incident_gate(results)
        self.assertTrue(gate["evaluated"])
        self.assertTrue(gate["passed"])
        results[0]["shareIntentSuccess"] = False
        self.assertFalse(incident_gate(results)["passed"])

    def test_each_cli_exposes_only_its_measured_browser_cli(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            bin_dir = Path(directory) / "bin"
            write_proxies(bin_dir, "pw017")
            self.assertTrue((bin_dir / "playwright-cli").exists())
            self.assertFalse((bin_dir / "agent-browser").exists())
        with tempfile.TemporaryDirectory() as directory:
            bin_dir = Path(directory) / "bin"
            write_proxies(bin_dir, "agent032")
            self.assertTrue((bin_dir / "agent-browser").exists())
            self.assertFalse((bin_dir / "playwright-cli").exists())

    def test_factorial_matrix_varies_cli_policy_vision_mode_independently(self) -> None:
        episodes = list(matrix())
        self.assertEqual(
            len(episodes),
            len(CLIS)
            * len(POLICIES)
            * len(VISIONS)
            * len(TASK_MODES)
            * 2
            * len(SEEDS)
            * 2,
        )

    def test_matched_capability_projection_has_one_capability_per_task_mode(self) -> None:
        matched = [
            episode
            for episode in matrix()
            if episode.vision == ("on" if episode.task_mode == "visual" else "off")
        ]
        self.assertEqual(len(matched), len(list(matrix())) // 2)

    def test_cli_cards_use_real_url_and_never_literal_url_placeholder(self) -> None:
        for cli in CLIS:
            text = render_skill(
                cli,
                "closed-loop",
                "on",
                "http://127.0.0.1:4321",
                "/tmp/observations",
                "/tmp/deliverables",
            )
            self.assertIn("http://127.0.0.1:4321", text)
            self.assertNotIn("open URL", text)

    def test_cli_cards_cover_the_same_core_operations(self) -> None:
        playwright = render_skill(
            "pw017", "minimal", "on", "TASK", "OBS", "FINAL"
        ).split("## CLI command card", 1)[1]
        agent = render_skill(
            "agent032", "minimal", "on", "TASK", "OBS", "FINAL"
        ).split("## CLI command card", 1)[1]
        for operation in ("open", "snapshot", "click", "fill", "select", "screenshot"):
            with self.subTest(operation=operation):
                self.assertIn(f"playwright-cli {operation}", playwright)
                self.assertIn(f"agent-browser {operation}", agent)

    def test_policy_and_sharing_text_are_identical_across_cli_cards(self) -> None:
        playwright = render_skill(
            "pw017", "closed-loop", "on", "TASK", "OBS", "FINAL"
        )
        agent = render_skill(
            "agent032", "closed-loop", "on", "TASK", "OBS", "FINAL"
        )
        playwright_common = playwright.split("## CLI command card", 1)[0]
        agent_common = agent.split("## CLI command card", 1)[0]
        playwright_common = playwright_common.replace(
            "browser-eval-pw017-closed-loop-on", "browser-eval-CLI-closed-loop-on"
        ).replace("playwright-cli 0.1.17", "TOOL VERSION")
        agent_common = agent_common.replace(
            "browser-eval-agent032-closed-loop-on", "browser-eval-CLI-closed-loop-on"
        ).replace("agent-browser 0.32.0", "TOOL VERSION")
        self.assertEqual(playwright_common, agent_common)

    def test_candidate_is_a_discovered_skill_not_root_instructions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            skill_path = install_candidate_skills(workspace, "---\nname: browser\n---\n")
            self.assertEqual(skill_path, workspace / ".agents/skills/browser/SKILL.md")
            self.assertTrue((workspace / ".agents/skills/share-file/SKILL.md").is_file())
            self.assertEqual(
                (workspace / ".claude/skills").resolve(),
                (workspace / ".agents/skills").resolve(),
            )
            self.assertFalse((workspace / "AGENTS.md").exists())
            self.assertFalse((workspace / "CLAUDE.md").exists())

    def test_episode_prompt_does_not_disclose_eval_factor_or_artifact_paths(self) -> None:
        prompt = episode_prompt("http://127.0.0.1:4321", 0, False, "dom")
        self.assertNotIn("observation mode", prompt.lower())
        self.assertNotIn("screenshot directory", prompt.lower())
        self.assertNotIn("deliverable directory", prompt.lower())
        self.assertNotIn("evaluation", prompt.lower())

    def test_task_modes_express_honest_user_intent(self) -> None:
        self.assertIn("event log", task_prompt(0, False, mode="dom").lower())
        self.assertNotIn("topology", task_prompt(0, False, mode="dom").lower())
        self.assertIn("topology", task_prompt(0, False, mode="visual").lower())
        self.assertIn("arrow", task_prompt(0, False, mode="visual").lower())

    def test_candidate_skill_does_not_announce_that_it_is_an_eval(self) -> None:
        for cli in CLIS:
            for policy in POLICIES:
                text = render_skill(cli, policy, "on", "URL", "OBS", "FINAL")
                self.assertNotIn("measured", text.lower())
                self.assertNotIn("evaluation", text.lower())

    def test_skill_template_hash_ignores_only_episode_locations(self) -> None:
        first = render_skill("pw017", "closed-loop", "on", "URL1", "OBS1", "FINAL1")
        second = render_skill("pw017", "closed-loop", "on", "URL2", "OBS2", "FINAL2")
        self.assertEqual(
            normalized_skill_hash(first, "URL1", Path("OBS1"), Path("FINAL1")),
            normalized_skill_hash(second, "URL2", Path("OBS2"), Path("FINAL2")),
        )

    def test_visual_interlock_detects_internal_screenshots_symmetrically(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            playwright_image = root / "pw.png"
            agent_image = root / "agent.png"
            playwright_image.write_bytes(b"png")
            agent_image.write_bytes(b"png")
            self.assertEqual(
                screenshot_path(
                    "playwright-cli", ["screenshot", "e5", "--filename", str(playwright_image)]
                ),
                playwright_image,
            )
            self.assertEqual(
                screenshot_path(
                    "agent-browser", ["screenshot", "--annotate", str(agent_image)]
                ),
                agent_image,
            )
            self.assertEqual(
                internal_screenshot_path(
                    "playwright-cli",
                    ["screenshot", "e5", "--filename", str(playwright_image)],
                    root,
                ),
                playwright_image.resolve(),
            )
            self.assertEqual(
                internal_screenshot_path(
                    "agent-browser",
                    ["screenshot", "--annotate", str(agent_image)],
                    root,
                ),
                agent_image.resolve(),
            )

    def test_private_screenshot_announces_required_visual_handoff(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            observations = root / "observations"
            observations.mkdir()
            image = observations / "page.png"
            image.write_bytes(b"\x89PNG\r\n\x1a\n")
            driver = root / "driver.json"
            driver.write_text(
                json.dumps(
                    {
                        "cdpPort": 1,
                        "sessionName": "unit",
                        "commands": {"agent-browser": "/usr/bin/true"},
                    }
                )
            )
            pending = root / "pending"
            previous = os.environ.copy()
            try:
                os.environ.update(
                    {
                        "ENGRAM_EVAL_EVENTS": str(root / "events.jsonl"),
                        "ENGRAM_EVAL_ACTION_COUNT": str(root / "actions"),
                        "ENGRAM_EVAL_ACTION_LIMIT": "30",
                        "ENGRAM_EVAL_VISUAL_GATE": "1",
                        "ENGRAM_EVAL_PENDING_VIEW": str(pending),
                        "ENGRAM_EVAL_OBSERVATION_ROOT": str(observations),
                        "ENGRAM_EVAL_DRIVER_CONFIG": str(driver),
                    }
                )
                output = StringIO()
                with redirect_stdout(output):
                    self.assertEqual(browser("agent-browser", ["screenshot", str(image)]), 0)
                self.assertIn("Call browser_view with this exact path now", output.getvalue())
                self.assertEqual(pending.read_text(), str(image.resolve()))
            finally:
                os.environ.clear()
                os.environ.update(previous)

    def test_vision_off_blocks_internal_but_allows_deliverable_screenshots(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            observations = root / "observations"
            deliverables = root / "deliverables"
            observations.mkdir()
            deliverables.mkdir()
            previous = os.environ.copy()
            try:
                os.environ.update(
                    {
                        "ENGRAM_EVAL_VISUAL_GATE": "0",
                        "ENGRAM_EVAL_OBSERVATION_ROOT": str(observations),
                        "ENGRAM_EVAL_DELIVERABLE_ROOT": str(deliverables),
                        "ENGRAM_EVAL_EVIDENCE_REQUESTED": "1",
                    }
                )
                self.assertIsNotNone(
                    screenshot_policy_error(
                        "agent-browser", ["screenshot", str(observations / "page.png")]
                    )
                )
                self.assertIsNone(
                    screenshot_policy_error("agent-browser", ["screenshot", "--help"])
                )
                self.assertIsNone(
                    screenshot_policy_error(
                        "agent-browser", ["screenshot", str(deliverables / "final.png")]
                    )
                )
                self.assertIsNotNone(
                    screenshot_policy_error(
                        "playwright-cli", ["screenshot", "--filename", "/tmp/escape.png"]
                    )
                )
            finally:
                os.environ.clear()
                os.environ.update(previous)

    def test_vision_off_skill_does_not_advertise_internal_screenshots(self) -> None:
        for cli in CLIS:
            text = render_skill(cli, "minimal", "off", "URL", "OBS", "FINAL")
            command_card = text.split("## CLI command card", 1)[1]
            self.assertNotIn("OBS", command_card)
            self.assertIn("FINAL", command_card)

    def test_visual_skill_makes_the_pixel_handoff_unambiguous(self) -> None:
        for cli in CLIS:
            text = render_skill(cli, "closed-loop", "on", "URL", "OBS", "FINAL")
            self.assertIn("very next\ntool call must be `browser_view`", text)
            self.assertIn("does not supply pixels", text)
            self.assertIn("Do not\ntake another screenshot to re-check semantic success", text)

    def test_eval_viewport_matches_production_geometry(self) -> None:
        command = chrome_command(Path("/tmp/profile"), 9222)
        self.assertIn("--window-size=1440,1080", command)
        self.assertIn("--window-position=0,0", command)

    def test_task_modes_are_explicit_and_observable_state_is_sanitized(self) -> None:
        for mode in TASK_MODES:
            reset = post(f"{self.server.url}/api/reset", {"seed": 0, "mode": mode})
            self.assertEqual(reset["mode"], mode)
            self.assertEqual(get(f"{self.server.url}/api/config")["mode"], mode)
        observable = get(f"{self.server.url}/api/observable")
        self.assertEqual(observable["view"], "list")
        self.assertNotIn("source", observable)

    def test_share_help_is_not_a_published_artifact(self) -> None:
        events = [
            {"type": "share", "argv": ["--help"]},
            {"type": "share_probe", "argv": ["--version"]},
            {"type": "share", "argv": ["--file", "/workspace/final.png"]},
        ]
        self.assertEqual(count_shared_artifacts(events), 1)

    def test_only_valid_share_events_count_as_delivered_evidence(self) -> None:
        events = [
            {"type": "share", "valid": False},
            {"type": "share", "valid": True},
            {"type": "share_probe", "valid": True},
        ]
        self.assertEqual(count_valid_shared_artifacts(events), 1)

    def test_share_shim_requires_real_post_success_deliverable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            deliverables = root / "deliverables"
            deliverables.mkdir()
            image = deliverables / "final.png"
            image.write_bytes(b"\x89PNG\r\n\x1a\n" + b"proof")
            events = root / "events.jsonl"
            previous = os.environ.copy()
            try:
                os.environ.update(
                    {
                        "ENGRAM_EVAL_EVENTS": str(events),
                        "ENGRAM_EVAL_DELIVERABLE_ROOT": str(deliverables),
                        "ENGRAM_EVAL_GRADE_URL": f"{self.server.url}/api/grade",
                    }
                )
                self.assertEqual(share(["--file", str(image)]), 2)
                cfg = SEEDS[0]
                post(f"{self.server.url}/api/reset", {"seed": 0, "mode": "dom"})
                post(f"{self.server.url}/api/observe", {"filter": cfg["incident"]})
                post(f"{self.server.url}/api/select", {"incident": cfg["incident"]})
                post(
                    f"{self.server.url}/api/submit",
                    {"service": cfg["source"], "policy": cfg["policy"]},
                )
                self.assertEqual(share(["--file", str(image)]), 0)
                delayed = deliverables / "delayed.png"
                def create_delayed_image() -> None:
                    time.sleep(0.05)
                    delayed.write_bytes(b"\x89PNG\r\n\x1a\n" + b"proof")

                producer = Thread(target=create_delayed_image)
                producer.start()
                self.assertEqual(share(["--file", str(delayed)]), 0)
                producer.join(timeout=1)
                recorded = [json.loads(line) for line in events.read_text().splitlines()]
                self.assertEqual(count_shared_artifacts(recorded), 3)
                self.assertEqual(count_valid_shared_artifacts(recorded), 2)
            finally:
                os.environ.clear()
                os.environ.update(previous)

    def test_screenshot_probes_do_not_count_as_screenshots(self) -> None:
        events = [
            {"type": "browser_start", "argv": ["screenshot", "--help"]},
            {"type": "browser_start", "argv": ["snapshot"]},
            {"type": "browser_start", "argv": ["screenshot", "/tmp/page.png"]},
        ]
        self.assertEqual(count_screenshots(events), 1)

    def test_screenshot_roles_distinguish_private_and_deliverable_images(self) -> None:
        events = [
            {
                "type": "browser_start",
                "argv": ["screenshot", "/tmp/private.png"],
                "screenshotRole": "internal",
            },
            {
                "type": "browser_start",
                "argv": ["screenshot", "/tmp/final.png"],
                "screenshotRole": "deliverable",
            },
            {
                "type": "browser_start",
                "argv": ["screenshot", "--help"],
                "screenshotRole": "unclassified",
            },
        ]
        self.assertEqual(
            count_screenshot_roles(events),
            {"internal": 1, "deliverable": 1, "unclassified": 0},
        )

    def test_repeat_bound_counts_only_unchanged_mutations(self) -> None:
        def command(action: int, verb: str, before: str, after: str) -> list[dict[str, object]]:
            return [
                {
                    "type": "browser_start",
                    "action": action,
                    "tool": "agent-browser",
                    "argv": [verb, "@e1"],
                    "before": {"stateHash": before},
                },
                {
                    "type": "browser_end",
                    "action": action,
                    "after": {"stateHash": after},
                },
            ]

        events = []
        events += command(1, "snapshot", "a", "a")
        events += command(2, "snapshot", "a", "a")
        events += command(3, "click", "a", "a")
        events += command(4, "click", "a", "a")
        events += command(5, "click", "a", "b")
        self.assertEqual(max_identical_failed_attempts(events), 2)

        events += [
            {
                "type": "visual_gate",
                "tool": "agent-browser",
                "argv": ["select", "@e5", "value"],
                "requiredPath": "/tmp/decision.png",
            },
            {
                "type": "visual_gate",
                "tool": "agent-browser",
                "argv": ["select", "@e5", "value"],
                "requiredPath": "/tmp/decision.png",
            },
            {
                "type": "visual_gate",
                "tool": "agent-browser",
                "argv": ["snapshot"],
                "requiredPath": "/tmp/decision.png",
            },
        ]
        self.assertEqual(max_identical_failed_attempts(events), 2)

    def test_canvas_answer_introspection_is_blocked_for_both_clis(self) -> None:
        self.assertIsNotNone(introspection_block_reason("agent-browser", ["eval", "cfg.source"]))
        self.assertIsNotNone(
            introspection_block_reason("playwright-cli", ["run-code", "cfg.source"])
        )
        self.assertIsNotNone(
            introspection_block_reason("agent-browser", ["open", "http://task/api/config"])
        )
        self.assertIsNone(introspection_block_reason("agent-browser", ["snapshot", "-i"]))

    def test_candidates_cannot_override_episode_browser_session(self) -> None:
        self.assertEqual(
            remove_isolation_overrides(
                "agent-browser",
                ["--namespace", "other", "--cdp=1", "snapshot", "-i"],
            ),
            ["snapshot", "-i"],
        )
        self.assertEqual(
            remove_isolation_overrides("playwright-cli", ["-s=other", "snapshot"]),
            ["snapshot"],
        )

    def test_raw_browser_and_shell_http_are_protocol_violations(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "agent.jsonl"
            log.write_text(
                json.dumps(
                    {
                        "item": {
                            "type": "command_execution",
                            "command": "/tmp/raw/playwright-cli snapshot",
                        }
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            self.assertTrue(
                protocol_violation(
                    log, "/tmp/raw/playwright-cli", "http://127.0.0.1:1234"
                )
            )
            log.write_text(
                json.dumps(
                    {
                        "item": {
                            "type": "command_execution",
                            "command": "playwright-cli snapshot",
                        }
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            self.assertFalse(
                protocol_violation(
                    log, "/tmp/raw/playwright-cli", "http://127.0.0.1:1234"
                )
            )

    def test_codex_image_tool_allows_the_episode_observation_root(self) -> None:
        command = harness_command(
            "codex",
            Path("/tmp/workspace"),
            "prompt",
            Path("/tmp/mcp.json"),
            True,
            Path("/tmp/artifacts/episode"),
        )
        self.assertIn("/tmp/artifacts/episode", " ".join(command))
        self.assertNotIn("/tmp\"]", " ".join(command))

    def test_claude_uses_project_only_settings(self) -> None:
        command = harness_command(
            "claude",
            Path("/tmp/workspace"),
            "prompt",
            Path("/tmp/mcp.json"),
            False,
            Path("/tmp/artifacts/episode"),
        )
        index = command.index("--setting-sources")
        self.assertEqual(command[index + 1], "project,local")


if __name__ == "__main__":
    unittest.main()
