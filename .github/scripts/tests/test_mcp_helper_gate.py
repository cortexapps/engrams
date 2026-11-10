import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / '.github/scripts/mcp-acceptance-helper.sh'
TARGETS = [
    'managed::helper::tests::actual_helper_closed_lifetime_and_descriptor_isolation',
    'managed::helper::tests::actual_helper_release_ack_and_active_dns_cleanup',
    'managed::helper::tests::actual_helper_rejects_wrong_epoch_and_canceled_release',
]
FILTER = ' or '.join(f'test(={target})' for target in TARGETS)


class HelperGate(unittest.TestCase):
    def test_required_count_and_command_contract(self):
        # Fake cargo output only: no Rust test or helper process runs here.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / 'var/mcp-netstack-artifact'
            artifact.mkdir(parents=True)
            helper = artifact / 'engram-sandbox-netstack'
            helper.write_text('#!/bin/sh\nexit 97\n')
            helper.chmod(0o700)
            digest = hashlib.sha256(helper.read_bytes()).hexdigest()
            (artifact / 'SHA256SUMS').write_text(f'{digest}  {helper.name}\n')
            commands = root / 'bin'
            commands.mkdir()
            cargo = commands / 'cargo'
            cargo.write_text(f'#!{sys.executable}\n'
                             'import json, os, sys\n'
                             'from pathlib import Path\n'
                             'Path(os.environ["FAKE_CARGO_ARGS"]).write_text(json.dumps(sys.argv[1:]))\n'
                             'print(os.environ["FAKE_CARGO_SUMMARY"])\n'
                             'sys.exit(int(os.environ.get("FAKE_CARGO_EXIT", "0")))\n')
            cargo.chmod(0o700)
            args_file = root / 'args.json'
            env = dict(os.environ, PATH=f'{commands}{os.pathsep}{os.environ["PATH"]}',
                       FAKE_CARGO_ARGS=str(args_file))
            for count in [3, 2, 1, 0, 13]:
                with self.subTest(count=count):
                    env['FAKE_CARGO_SUMMARY'] = f'Summary [0.01s] {count} tests run: {count} passed, 37 skipped'
                    result = subprocess.run(['bash', str(SCRIPT)], cwd=root, env=env,
                                            capture_output=True, text=True)
                    self.assertEqual(result.returncode == 0, count == 3, result.stdout + result.stderr)
            self.assertEqual(json.loads(args_file.read_text()), [
                'nextest', 'run', '-p', 'engram-sandbox-vz', '--lib',
                '--run-ignored', 'ignored-only', '-E', FILTER,
                '--no-tests', 'fail', '--test-threads', '1',
            ])
            env['FAKE_CARGO_SUMMARY'] = 'Summary [0.01s] 3 tests run: 3 passed, 37 skipped'
            env['FAKE_CARGO_EXIT'] = '42'
            result = subprocess.run(['bash', str(SCRIPT)], cwd=root, env=env, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            del env['FAKE_CARGO_EXIT']
            for invalid in ['checksum', 'executable']:
                with self.subTest(artifact=invalid):
                    args_file.unlink(missing_ok=True)
                    if invalid == 'checksum':
                        helper.write_text('#!/bin/sh\nexit 98\n')
                    else:
                        helper.write_text('#!/bin/sh\nexit 97\n')
                        helper.chmod(0o600)
                    result = subprocess.run(['bash', str(SCRIPT)], cwd=root, env=env, capture_output=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(args_file.exists(), 'invalid artifact must fail before cargo')

    def test_workflow_owns_all_three_targets_once(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        lane = workflow.split('  vz:', 1)[1].split('  test-macos:', 1)[0]
        ignored = lane.split('      - name: Run ignored VZ tests', 1)[1].split('      - name:', 1)[0]
        exclusions = ' and '.join(f'not test(={target})' for target in TARGETS)
        self.assertIn(f"-E 'not test(e2e_vz) and {exclusions}'", ignored)
        gate = lane.split('      - name: Run verified packaged helper tests', 1)[1].split('      - name:', 1)[0]
        self.assertIn('        if: always()', gate)
        self.assertIn('run: bash .github/scripts/mcp-acceptance-helper.sh', gate)
        self.assertIn('needs: [detect, netstack]', lane)
        self.assertIn("if: needs.detect.outputs.test_rust == 'true'", lane)
        self.assertIn('name: sandbox-netstack-macos', lane)
        self.assertIn('shasum -a 256 -c SHA256SUMS', lane)
        self.assertIn('chmod +x engram-sandbox-netstack', lane)
        self.assertIn('      - vz', workflow.split('  ci-gate:', 1)[1])
        producer = workflow.split('  netstack:', 1)[1].split('  orchestrator:', 1)[0]
        self.assertIn('sandbox-netstack/build.sh build', producer)
        self.assertIn('shasum -a 256 engram-sandbox-netstack > SHA256SUMS', producer)
        self.assertIn('name: sandbox-netstack-${{ matrix.platform }}', producer)
        self.assertIn('if-no-files-found: error', producer)
        discovery = "python3 -m unittest discover -s .github/scripts/tests -p 'test_mcp*.py'"
        self.assertEqual(workflow.count(discovery), 1)
        detector = workflow.split('  detect:', 1)[1].split('      - name: Detect changed lanes', 1)[0]
        self.assertIn(f'run: {discovery}', detector)


if __name__ == '__main__':
    unittest.main()
