"""The shell wrappers pass each field=value to curl as exactly one argument."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import tempfile
import unittest

BIN = pathlib.Path(__file__).parents[1] / "bin"

# Records its argv as JSON and answers with an empty object.
FAKE_CURL = """#!/bin/sh
python3 -c 'import json, sys; print(json.dumps(sys.argv[1:]))' "$@" >> "$CURL_ARGV_LOG"
printf '{}'
"""


class WrapperArgs(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.tmp.name)
        curl = root / "curl"
        curl.write_text(FAKE_CURL)
        curl.chmod(0o755)
        self.log = root / "argv.jsonl"
        self.env = {
            "PATH": f"{root}:{os.environ['PATH']}",
            "CURL_ARGV_LOG": str(self.log),
        }

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def curl_argv(self, *command: str) -> list[str]:
        subprocess.run(
            [str(BIN / command[0]), *command[1:]],
            env=self.env,
            check=True,
            capture_output=True,
        )
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual(len(calls), 1)
        return calls[0]

    def fields(self, argv: list[str], flag: str = "--data-urlencode") -> list[str]:
        return [argv[i + 1] for i, arg in enumerate(argv) if arg == flag]

    def test_slack_api_keeps_a_multiline_field_whole(self) -> None:
        argv = self.curl_argv("slack", "api", "chat.update", "channel=C1", "text=line one\nline two")
        self.assertEqual(self.fields(argv), ["channel=C1", "text=line one\nline two"])
        self.assertIn("https://slack.com/api/chat.update", argv)

    def test_slack_history_keeps_fields_with_spaces(self) -> None:
        argv = self.curl_argv("slack", "history", "C1", "oldest=1 2")
        self.assertEqual(self.fields(argv), ["channel=C1", "oldest=1 2"])

    def test_vercel_rewrites_team_to_team_id(self) -> None:
        argv = self.curl_argv("vercel", "projects", "team=t1", "search=a b")
        self.assertEqual(self.fields(argv), ["teamId=t1", "search=a b"])

    def test_vanta_splits_a_matches_any_list(self) -> None:
        argv = self.curl_argv("vanta", "get", "people", "statusMatchesAny=A,B", "q=x\ny")
        self.assertEqual(self.fields(argv), ["statusMatchesAny=A", "statusMatchesAny=B", "q=x\ny"])

    def test_twilio_get_sends_query_fields(self) -> None:
        argv = self.curl_argv("twilio", "get", "/2010-04-01/Accounts.json", "FriendlyName=a\nb")
        self.assertIn("-G", argv)
        self.assertEqual(self.fields(argv), ["FriendlyName=a\nb"])

    def test_cortex_entities_adds_paging_defaults(self) -> None:
        argv = self.curl_argv("cortex", "entities", "query=a b")
        self.assertEqual(self.fields(argv), ["query=a b", "page=0", "pageSize=50"])


if __name__ == "__main__":
    unittest.main()
