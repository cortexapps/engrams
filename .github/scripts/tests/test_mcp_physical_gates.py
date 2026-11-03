import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location('physical_assets', ROOT / '.github/scripts/mcp-stack-assets.py')
assets = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(assets)


class PhysicalGates(unittest.TestCase):
    def test_source_context_rejects_secrets(self):
        assets.source_paths(['docker/host-agent.Dockerfile', '.env.example', 'orchestrator/src/index.ts'])
        for path in ['.env', 'orchestrator/.env.local', 'var/key', '.git/config', '../key', '/tmp/key']:
            with self.subTest(path=path), self.assertRaises(RuntimeError):
                assets.source_paths([path])

    def test_archive_identity_rejects_index_wrong_arch_and_digest(self):
        config = json.dumps({'os': 'linux', 'architecture': 'arm64'}).encode()
        raw = json.dumps({'schemaVersion': 2, 'config': {'digest': assets.digest(config)}}).encode()
        facts = assets.image_facts(raw, config, 'arm64')
        self.assertNotEqual(facts['id'], facts['manifestDigest'])
        for value, cfg, arch in [
            (raw, config, 'amd64'),
            (json.dumps({'schemaVersion': 2, 'manifests': []}).encode(), config, 'arm64'),
            (raw.replace(assets.digest(config).encode(), b'sha256:' + b'a' * 64), config, 'arm64'),
        ]:
            with self.assertRaises(RuntimeError):
                assets.image_facts(value, cfg, arch)

    def test_matrix_requires_actual_nonzero_output_and_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            completion = {'backend': 'vz', 'sourceCommit': 'a' * 40,
                          'matrixCompleted': True, 'cleanupFailed': []}
            (root / 'completion-1.json').write_text(json.dumps(completion))
            for passed, selected, ok in [(24, 24, True), (0, 0, False), (23, 24, False), (24, 0, False)]:
                (root / 'matrix.log').write_text(json.dumps({'status': 'PASS', 'selected': selected, 'passed': passed}) + '\n')
                if ok:
                    self.assertEqual(assets.matrix_result(root, 'vz', 'a' * 40), 24)
                else:
                    with self.assertRaises(RuntimeError):
                        assets.matrix_result(root, 'vz', 'a' * 40)
            (root / 'matrix.log').write_text('{"status":"PASS","selected":24,"passed":24}\n')
            for field, value in [('matrixCompleted', False), ('cleanupFailed', ['host']), ('sourceCommit', 'b' * 40)]:
                (root / 'completion-1.json').write_text(json.dumps({**completion, field: value}))
                with self.assertRaises(RuntimeError):
                    assets.matrix_result(root, 'vz', 'a' * 40)

    def test_operator_rejects_commands_credentials_and_noncanonical_socket(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            sock = socket.socket(socket.AF_UNIX)
            try:
                sock.bind(str(root / 'engine.sock'))
                tool = root / 'tool'
                tool.write_text('#!/bin/sh\nexit 0\n')
                tool.chmod(0o700)
                config = {'version': 1, 'backend': 'firecracker', 'engineSocket': str(root / 'engine.sock'),
                          'nativeHostIp': '192.168.1.2', 'subnet': '10.88.2.0/24', 'registryPort': 30000,
                          'buildRegistryPort': 31000, 'tools': {key: str(tool) for key in assets.TOOLS}}
                path = root / 'operator.json'
                path.write_text(json.dumps(config))
                path.chmod(0o600)
                self.assertEqual(assets.read_operator(str(path), 'firecracker'), config)
                for extra in [{'command': 'anything'}, {'token': 'secret'}, {'PASS': True}]:
                    path.write_text(json.dumps({**config, **extra}))
                    with self.assertRaises(RuntimeError):
                        assets.read_operator(str(path), 'firecracker')
                for key, value in [('engineSocket', str(root / 'missing')), ('subnet', '10.0.2.0/24'),
                                   ('nativeHostIp', '127.0.0.1'), ('buildRegistryPort', 30002)]:
                    path.write_text(json.dumps({**config, key: value}))
                    with self.assertRaises((RuntimeError, FileNotFoundError)):
                        assets.read_operator(str(path), 'firecracker')
            finally:
                sock.close()

    def test_command_failure_and_cancellation_stop_production(self):
        # Fake CLI child only. No engine, application, helper or VM is executed.
        with tempfile.TemporaryDirectory() as directory:
            p = object.__new__(assets.Producer)
            p.root = Path(directory)
            p.counter, p.cancelled, p.child = 0, False, None
            p.env = {'PATH': '/usr/bin:/bin', 'LANG': 'C'}
            with self.assertRaises(RuntimeError):
                p.call([sys.executable, '-c', 'raise SystemExit(42)'])
            p.cancelled = True
            with self.assertRaises(RuntimeError):
                p.call([sys.executable, '-c', 'raise SystemExit(0)'])
            self.assertEqual(p.counter, 1)
            self.assertIsNone(p.child)
            p.cancelled = False
            with self.assertRaises(subprocess.TimeoutExpired):
                p.call([sys.executable, '-c', 'import time; time.sleep(60)'], timeout=0.02)
            self.assertIsNone(p.child)

    def test_fake_engine_uses_explicit_selection_and_rejects_identity_change(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            sock = socket.socket(socket.AF_UNIX)
            try:
                sock.bind(str(root / 'engine.sock'))
                fake = root / 'docker'
                fake.write_text(f'#!{sys.executable}\n'
                                'import json, os, sys\n'
                                'from pathlib import Path\n'
                                f'root = Path({str(root)!r})\n'
                                'assert "HOME" not in os.environ and "GITHUB_TOKEN" not in os.environ\n'
                                'with (root / "calls").open("a") as out: out.write(json.dumps(sys.argv[1:]) + "\\n")\n'
                                'if "info" in sys.argv: print(json.dumps({"ID": (root / "id").read_text()}))\n'
                                'else: print("[]")\n')
                fake.chmod(0o700)
                (root / 'id').write_text('engine-one-identity')
                o = {'engineSocket': str(root / 'engine.sock'), 'tools': {'docker': str(fake)}}
                p = assets.Producer('firecracker', root, o)
                p.engine_identity = 'engine-one-identity'
                p.engine('image', 'inspect', 'sha256:' + 'a' * 64)
                calls = [json.loads(line) for line in (root / 'calls').read_text().splitlines()]
                self.assertEqual(len(calls), 3)
                for call in calls:
                    self.assertEqual(call[:4], ['--config', str(p.config), '--host', 'unix://' + str(root / 'engine.sock')])
                (root / 'id').write_text('engine-two-identity')
                with self.assertRaises(RuntimeError):
                    p.engine('create', 'anything')
                calls = [json.loads(line) for line in (root / 'calls').read_text().splitlines()]
                self.assertFalse(any('create' in call for call in calls))
            finally:
                sock.close()

    def test_cleanup_refuses_foreign_resource_and_survivor(self):
        with tempfile.TemporaryDirectory() as directory:
            for foreign, survivor in [(True, False), (False, True), (False, False)]:
                root = Path(directory) / f'{foreign}-{survivor}'
                root.mkdir()
                p = object.__new__(assets.Producer)
                p.root, p.run, p.names = root, 'mcp-ci-owned', ['mcp-ci-owned-registry']
                calls = []
                def fake(*args, **kwargs):
                    calls.append(args)
                    if args[0] == 'ps':
                        return b'a' * 64 if len(calls) == 1 or survivor else b''
                    if args[0] == 'inspect':
                        return json.dumps([{'Name': '/mcp-ci-owned-registry', 'Config': {'Labels': {
                            assets.LABEL: 'foreign' if foreign else p.run}}}]).encode()
                    return b''
                p.engine = fake
                if foreign or survivor:
                    with self.assertRaises(RuntimeError):
                        p.cleanup()
                else:
                    p.cleanup()
                self.assertEqual(any(call[0] == 'rm' for call in calls), not foreign)

    def test_adapter_cleanup_requires_actual_receipt_and_survivor_queries(self):
        with tempfile.TemporaryDirectory() as directory:
            for mode in ['good', 'missing', 'failed', 'survivor']:
                root = Path(directory) / mode
                root.mkdir()
                (root / 'run').mkdir()
                p = object.__new__(assets.Producer)
                p.root, p.run, p.names = root, 'mcp-ci-owned', []
                p.backend, p.head = 'vz', 'a' * 40
                p.adapter_started, p.adapter_clean = True, False
                if mode != 'missing':
                    (root / 'run/completion-1.json').write_text(json.dumps({
                        'backend': 'vz', 'sourceCommit': p.head,
                        'cleanupFailed': ['host'] if mode == 'failed' else []}))
                    (root / 'run/cleanup.json').write_text(json.dumps({
                        'success': True, 'containersRemaining': 0, 'networkRemaining': False,
                        'runId': 'mcp-' + 'b' * 24}))
                queries = []
                def fake(*args, **kwargs):
                    queries.append(args)
                    return b'owned-survivor' if mode == 'survivor' else b''
                p.engine = fake
                if mode == 'good':
                    p.cleanup()
                    self.assertTrue(p.adapter_clean)
                    self.assertEqual(len(queries), 2)
                else:
                    with self.assertRaises(RuntimeError):
                        p.cleanup()
                    self.assertFalse(p.adapter_clean)
                self.assertTrue((root / 'producer-cleanup.json').is_file())

    def test_workflow_graph_has_no_skipped_producers_and_safe_uploads(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        for backend, name, label, operator in [
            ('firecracker', 'fc', 'engram-mcp-firecracker', '/etc/engrams/mcp-physical-firecracker.json'),
            ('vz', 'vz', 'engram-mcp-vz', '/Library/Application Support/engrams/mcp-physical-vz.json'),
        ]:
            # Extract to next two-space job key, not nested YAML fields.
            lane = workflow.split(f'  mcp-physical-{name}:', 1)[1].split('\n  mcp-physical-', 1)[0].split('\n  ci-gate:', 1)[0]
            self.assertIn('needs: detect', lane)
            self.assertNotIn('needs: [', lane)
            self.assertIn("if: needs.detect.outputs.test_mcp_physical == 'true'", lane)
            self.assertIn(label, lane)
            self.assertIn(f'--backend {backend}', lane)
            self.assertIn(operator, lane)
            self.assertIn('persist-credentials: false', lane)
            self.assertIn('if-no-files-found: error', lane)
            self.assertIn(f'      - mcp-physical-{name}\n', workflow.split('  ci-gate:', 1)[1])
            self.assertNotIn('continue-on-error', lane)
            self.assertNotIn('fc-setup', lane)
        source = (ROOT / '.github/scripts/mcp-stack-assets.py').read_text()
        self.assertIn("'--run-root', self.root / 'run'", source)
        self.assertIn("'--artifacts', self.root / 'inputs.json'", source)
        self.assertIn("'--experimental-transform-types'", source)
        self.assertNotIn("os.environ['HOME']", source)
        self.assertNotIn('prune', source)


if __name__ == '__main__':
    unittest.main()
