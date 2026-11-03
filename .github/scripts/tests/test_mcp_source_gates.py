import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / '.github/scripts/mcp-acceptance-native.sh'
DEADLINE = 'evidence::tests::expired_deadline_cannot_consume_a_release'


class NativeSourceGate(unittest.TestCase):
    def test_binary_protocol_and_deadline_guards(self):
        # Command stubs only: no Rust test, fixture, or harness process runs.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = root / 'bin'
            commands.mkdir()
            target = root / 'target'
            (target / 'debug').mkdir(parents=True)
            fixture = target / 'debug/engram-mcp-model-fixture'
            fixture.write_text('#!/bin/sh\nprintf "engram-mcp-model-fixture 1\\n"\n')
            fixture.chmod(0o700)
            cargo = commands / 'cargo'
            cargo.write_text(f'#!{sys.executable}\n'
                             'import json, os, sys\n'
                             'from pathlib import Path\n'
                             'with Path(os.environ["FAKE_CARGO_ARGS"]).open("a") as log:\n'
                             '    log.write(json.dumps(sys.argv[1:]) + "\\n")\n'
                             'if "--test" in sys.argv:\n'
                             '    print(os.environ["FAKE_CARGO_SUMMARY"])\n'
                             '    sys.exit(int(os.environ.get("FAKE_CARGO_EXIT", "0")))\n'
                             'if "test" in sys.argv:\n'
                             '    print("test result: ok. 1 passed; 0 failed; 0 ignored")\n')
            cargo.chmod(0o700)
            args_file = root / 'args.jsonl'
            env = dict(PATH=f'{commands}{os.pathsep}/usr/bin:/bin',
                       CARGO_TARGET_DIR=str(target), FAKE_CARGO_ARGS=str(args_file))
            good = ('Running unittests src/main.rs (target/debug/deps/fixture)\n'
                    f'test {DEADLINE} ... ok\n'
                    'test result: ok. 4 passed; 0 failed; 0 ignored\n'
                    'Running tests/protocol.rs (target/debug/deps/protocol)\n'
                    'test result: ok. 38 passed; 0 failed; 0 ignored\n')
            for name, summary, exit_code, expected in [
                ('positive', good, 0, True),
                ('zero-protocol', good.replace('38 passed', '0 passed'), 0, False),
                ('missing-deadline', good.replace(f'test {DEADLINE} ... ok\n', ''), 0, False),
                ('failed-protocol', good.replace('38 passed; 0 failed', '37 passed; 1 failed'), 0, False),
                ('failed-command', good, 42, False),
            ]:
                with self.subTest(name=name):
                    args_file.unlink(missing_ok=True)
                    env.update(FAKE_CARGO_SUMMARY=summary, FAKE_CARGO_EXIT=str(exit_code))
                    result = subprocess.run(['bash', str(SCRIPT)], cwd=root, env=env,
                                            capture_output=True, text=True)
                    self.assertEqual(result.returncode == 0, expected, result.stdout + result.stderr)
                    args = [json.loads(line) for line in args_file.read_text().splitlines()]
                    self.assertEqual(args[0], [
                        'test', '--locked', '--manifest-path',
                        'tests/fixtures/mcp-model-cli/Cargo.toml', '--bins', '--test', 'protocol',
                    ])
                    self.assertEqual(len(args), 4 if expected else 1)

    def test_bun_source_gate_rejects_zero_and_failed_runs(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        step = workflow.split('      - name: MCP acceptance scripts unit tests', 1)[1]
        body = textwrap.dedent(step.split('        run: |\n', 1)[1].split('      - name:', 1)[0])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'orchestrator').mkdir()
            commands = root / 'bin'
            commands.mkdir()
            bun = commands / 'bun'
            bun.write_text(f'#!{sys.executable}\n'
                           'import os, sys\n'
                           'assert sys.argv[1:] == ["test", "scripts/mcp-stack"]\n'
                           'print(os.environ["FAKE_BUN_SUMMARY"])\n'
                           'sys.exit(int(os.environ["FAKE_BUN_EXIT"]))\n')
            bun.chmod(0o700)
            env = dict(PATH=f'{commands}{os.pathsep}/usr/bin:/bin', TMPDIR=str(root))
            for summary, exit_code, expected in [
                (' 64 pass\n 0 fail\nRan 64 tests across 6 files. [80ms]', 0, True),
                (' 0 pass\n 0 fail\nRan 0 tests across 6 files. [80ms]', 0, False),
                (' 63 pass\n 1 fail\nRan 64 tests across 6 files. [80ms]', 0, False),
                ('No tests found', 0, False),
                (' 64 pass\n 0 fail\nRan 64 tests across 6 files. [80ms]', 42, False),
            ]:
                with self.subTest(summary=summary, exit_code=exit_code):
                    env.update(FAKE_BUN_SUMMARY=summary, FAKE_BUN_EXIT=str(exit_code))
                    result = subprocess.run(['bash', '-c', body], cwd=root / 'orchestrator',
                                            env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode == 0, expected, result.stdout + result.stderr)
                    self.assertTrue((root / 'var/mcp-stack-runner').is_dir())

    def test_inputs_writer_gate_rejects_zero_failure_and_cancel(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        step = workflow.split('      - name: MCP Inputs writer source tests', 1)[1]
        body = textwrap.dedent(step.split('        run: |\n', 1)[1].split('      - name:', 1)[0])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = root / 'bin'
            commands.mkdir()
            node = commands / 'node'
            node.write_text(f'#!{sys.executable}\n'
                            'import os, sys\n'
                            'assert sys.argv[1:] == ["--experimental-transform-types", "--test", "--test-isolation=none", "../.github/scripts/tests/mcp-stack-inputs.test.mts"]\n'
                            'print(os.environ["FAKE_SUMMARY"])\n'
                            'sys.exit(int(os.environ["FAKE_EXIT"]))\n')
            node.chmod(0o700)
            env = dict(PATH=f'{commands}{os.pathsep}/usr/bin:/bin', TMPDIR=str(root))
            good = '# pass 38\n# fail 0\n# cancelled 0'
            for summary, code, expected in [(good, 0, True), (good.replace('38', '0'), 0, False),
                                             (good.replace('fail 0', 'fail 1'), 0, False),
                                             (good.replace('cancelled 0', 'cancelled 1'), 0, False),
                                             (good, 42, False), ('No tests', 0, False)]:
                env.update(FAKE_SUMMARY=summary, FAKE_EXIT=str(code))
                result = subprocess.run(['bash', '-c', body], cwd=root, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, expected, result.stdout + result.stderr)

    def test_platform_gate_requires_openssl_three_and_nonzero_tests(self):
        # Command stubs only: actual platform source tests run in the CI job.
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        step = workflow.split('      - name: MCP platform source unit tests', 1)[1]
        body = textwrap.dedent(step.split('        run: |\n', 1)[1].split('      - name:', 1)[0])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = root / 'bin'
            commands.mkdir()
            openssl = commands / 'openssl'
            openssl.write_text(f'#!{sys.executable}\n'
                               'import os, sys\n'
                               'assert sys.argv[1:] == ["version"]\n'
                               'print(os.environ["FAKE_OPENSSL_VERSION"])\n'
                               'sys.exit(int(os.environ["FAKE_OPENSSL_EXIT"]))\n')
            openssl.chmod(0o700)
            bun = commands / 'bun'
            bun.write_text(f'#!{sys.executable}\n'
                           'import os, sys\n'
                           'from pathlib import Path\n'
                           'assert sys.argv[1:] == ["test", "../tests/fixtures/mcp-stack/platform"]\n'
                           'assert os.environ["MCP_PLATFORM_TEST_OPENSSL"] == str(Path(__file__).with_name("openssl"))\n'
                           'print(os.environ["FAKE_BUN_SUMMARY"])\n'
                           'sys.exit(int(os.environ["FAKE_BUN_EXIT"]))\n')
            bun.chmod(0o700)
            env = dict(PATH=f'{commands}{os.pathsep}/usr/bin:/bin', TMPDIR=str(root))
            good = ' 22 pass\n 0 fail\nRan 22 tests across 1 file. [80ms]'
            for name, version, openssl_exit, summary, bun_exit, expected in [
                ('positive', 'OpenSSL 3.0.13', 0, good, 0, True),
                ('libressl', 'LibreSSL 3.3.6', 0, good, 0, False),
                ('old-openssl', 'OpenSSL 1.1.1', 0, good, 0, False),
                ('failed-openssl', 'OpenSSL 3.0.13', 42, good, 0, False),
                ('zero-tests', 'OpenSSL 3.0.13', 0, ' 0 pass\n 0 fail\nRan 0 tests across 1 file.', 0, False),
                ('failed-test', 'OpenSSL 3.0.13', 0, ' 21 pass\n 1 fail\nRan 22 tests across 1 file.', 0, False),
                ('no-tests', 'OpenSSL 3.0.13', 0, 'No tests found', 0, False),
                ('failed-command', 'OpenSSL 3.0.13', 0, good, 42, False),
            ]:
                with self.subTest(name=name):
                    env.update(FAKE_OPENSSL_VERSION=version, FAKE_OPENSSL_EXIT=str(openssl_exit),
                               FAKE_BUN_SUMMARY=summary, FAKE_BUN_EXIT=str(bun_exit))
                    result = subprocess.run(['bash', '-c', body], cwd=root, env=env,
                                            capture_output=True, text=True)
                    self.assertEqual(result.returncode == 0, expected, result.stdout + result.stderr)


if __name__ == '__main__':
    unittest.main()
