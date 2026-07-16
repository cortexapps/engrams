import json
import os
from pathlib import Path
import tempfile
import unittest
from urllib.request import Request, urlopen

from incident_console import SEEDS, start_server, task_prompt
from image_mcp import handle
from browser_eval import (
    count_shared_artifacts,
    harness_command,
    incident_gate,
    matrix,
    protocol_violation,
    read_line_with_timeout,
    write_proxies,
)
from tool_proxy import introspection_block_reason, remove_isolation_overrides


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

    def test_subprocess_observation_reads_are_bounded(self) -> None:
        read_fd, write_fd = os.pipe()
        try:
            with os.fdopen(read_fd) as reader:
                with self.assertRaisesRegex(TimeoutError, "driver response"):
                    read_line_with_timeout(reader, 0.01, "driver response")
        finally:
            os.close(write_fd)

    def test_each_seed_requires_exact_state(self) -> None:
        for seed, cfg in enumerate(SEEDS):
            with self.subTest(seed=seed):
                post(f"{self.server.url}/api/reset", {"seed": seed})
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

    def test_shipping_gate_requires_complete_matrix_and_exact_intent(self) -> None:
        results = []
        for episode in matrix():
            success = episode.arm == "hybrid"
            results.append(
                {
                    "arm": episode.arm,
                    "harness": episode.harness,
                    "success": success,
                    "taskSuccess": success,
                    "shareIntentSuccess": True,
                    "timedOut": False,
                    "protocolViolation": False,
                    "actions": 8,
                    "maxIdenticalRepeat": 0,
                }
            )
        gate = incident_gate(results)
        self.assertTrue(gate["evaluated"])
        self.assertTrue(gate["passed"])
        results[0]["shareIntentSuccess"] = False
        # Baseline intent failures are comparison data; candidate policy is the
        # shipping requirement.
        self.assertTrue(incident_gate(results)["passed"])
        hybrid = next(result for result in results if result["arm"] == "hybrid")
        hybrid["shareIntentSuccess"] = False
        self.assertFalse(incident_gate(results)["passed"])

    def test_each_arm_exposes_only_its_measured_browser_cli(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            bin_dir = Path(directory) / "bin"
            write_proxies(bin_dir, "prod")
            self.assertTrue((bin_dir / "playwright-cli").exists())
            self.assertFalse((bin_dir / "agent-browser").exists())
        with tempfile.TemporaryDirectory() as directory:
            bin_dir = Path(directory) / "bin"
            write_proxies(bin_dir, "hybrid")
            self.assertTrue((bin_dir / "agent-browser").exists())
            self.assertFalse((bin_dir / "playwright-cli").exists())

    def test_share_help_is_not_a_published_artifact(self) -> None:
        events = [
            {"type": "share", "argv": ["--help"]},
            {"type": "share_probe", "argv": ["--version"]},
            {"type": "share", "argv": ["--file", "/workspace/final.png"]},
        ]
        self.assertEqual(count_shared_artifacts(events), 1)

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


if __name__ == "__main__":
    unittest.main()
