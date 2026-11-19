"""The shell wrappers make the curl calls their commands promise: each field=value
is one argument, and `slack upload` follows Slack's two-step upload flow."""

from __future__ import annotations

import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

BIN = pathlib.Path(__file__).parents[1] / "bin"

# Records its argv as JSON. Answers Slack's upload ticket request with a
# ticket (slashes escaped, as Slack sends them) and anything else with `{}`.
FAKE_CURL = f"""#!{sys.executable}
import json, os, sys
with open(os.environ["CURL_ARGV_LOG"], "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\\n")
if any(arg.endswith("/files.getUploadURLExternal") for arg in sys.argv):
    print('{{"ok":true,"upload_url":"https:\\\\/\\\\/files.slack.com\\\\/upload\\\\/v1\\\\/abc","file_id":"F1"}}')
else:
    print("{{}}")
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

    def curl_calls(self, *command: str) -> list[list[str]]:
        subprocess.run(
            [str(BIN / command[0]), *command[1:]],
            env=self.env,
            check=True,
            capture_output=True,
        )
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def curl_argv(self, *command: str) -> list[str]:
        calls = self.curl_calls(*command)
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

    def assert_slack_upload(self) -> None:
        upload = pathlib.Path(self.tmp.name) / "report.txt"
        upload.write_text("hello")
        ticket, send, complete = self.curl_calls("slack", "upload", "C1", str(upload), 'Q3 "final"')
        self.assertIn("https://slack.com/api/files.getUploadURLExternal", ticket)
        self.assertEqual(self.fields(ticket), ["filename=report.txt", "length=5"])
        self.assertIn("https://files.slack.com/upload/v1/abc", send)
        self.assertIn(f"filename=@{upload}", send)
        self.assertIn("https://slack.com/api/files.completeUploadExternal", complete)
        files, channel = self.fields(complete)
        self.assertEqual(json.loads(files.removeprefix("files=")), [{"id": "F1", "title": 'Q3 "final"'}])
        self.assertEqual(channel, "channel_id=C1")

    def test_slack_upload_uses_the_external_upload_flow(self) -> None:
        if shutil.which("jq") is None:
            self.skipTest("jq is not installed")
        self.assert_slack_upload()

    def test_slack_upload_without_jq(self) -> None:
        # A PATH with only the tools the wrapper needs, and no jq.
        tools = pathlib.Path(self.tmp.name) / "tools"
        tools.mkdir()
        for tool in ["sh", "sed", "basename", "wc", "tr", "cat"]:
            (tools / tool).symlink_to(shutil.which(tool))
        self.env["PATH"] = f"{self.tmp.name}:{tools}"
        self.assert_slack_upload()

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
