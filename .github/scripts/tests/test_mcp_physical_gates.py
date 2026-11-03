import importlib.util
import json
import os
import signal
import time
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

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
            p.counter, p.cancelled, p.child, p.groups = 0, False, None, {}
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

    def test_private_checkout_covers_git_ownership_without_shared_trust(self):
        # Actual Git's ownership-test switch models runner UID -> root UID.
        # No sudo/chown/foreign account or global configuration is used.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            source = root / 'runner-checkout'
            source.mkdir()
            env = {'PATH': '/usr/bin:/bin', 'LANG': 'C', 'GIT_CONFIG_NOSYSTEM': '1',
                   'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_SYSTEM': '/dev/null',
                   'GIT_OPTIONAL_LOCKS': '0', 'GIT_NO_LAZY_FETCH': '1'}
            def git(*args, **kwargs):
                return subprocess.run(['/usr/bin/git', *args], env=kwargs.pop('env', env),
                                      capture_output=True, text=True, check=True, **kwargs)
            empty = root / 'empty-template'
            empty.mkdir()
            git('init', '--template=' + str(empty), str(source))
            (source / 'tracked.txt').write_text('exact public source\n')
            git('-C', str(source), 'add', 'tracked.txt')
            git('-C', str(source), '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                '-c', 'commit.gpgsign=false', 'commit', '-m', 'fixture')
            head = git('-C', str(source), 'rev-parse', 'HEAD').stdout.strip()
            # A configuration credential and hook are not part of the checkout.
            git('-C', str(source), 'config', '--local', 'http.extraHeader', 'fixture-sensitive-header')
            (source / '.git/hooks').mkdir(exist_ok=True)
            (source / '.git/hooks/post-checkout').write_text('fixture hook must not copy')
            config_before = (source / '.git/config').read_bytes()
            index_before = (source / '.git/index').read_bytes()
            refused = subprocess.run(['/usr/bin/git', '-C', str(source), 'status', '--porcelain'],
                                     env={**env, 'GIT_TEST_ASSUME_DIFFERENT_OWNER': '1'}, capture_output=True)
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn(b'dubious ownership', refused.stderr)
            work = root / 'private'
            work.mkdir(mode=0o700)
            p = object.__new__(assets.Producer)
            p.root, p.env = work, env
            p.counter, p.cancelled, p.child, p.groups = 0, False, None, {}
            upload = root / 'owned-upload-pack'
            upload.write_text('#!/bin/sh\nexec env GIT_TEST_ASSUME_DIFFERENT_OWNER=1 /usr/bin/git upload-pack "$@"\n')
            upload.chmod(0o700)
            original_call = p.call
            def ownership_call(argv, **kwargs):
                words = [str(x) for x in argv]
                if '-C' in words and words[words.index('-C') + 1] == str(source):
                    kwargs['env'] = {**env, 'GIT_TEST_ASSUME_DIFFERENT_OWNER': '1'}
                if 'fetch' in words:
                    words.insert(words.index('fetch') + 1, '--upload-pack=' + str(upload))
                return original_call(words, **kwargs)
            p.call = ownership_call
            checkout, actual = p.private_checkout(source)
            self.assertEqual(actual, head)
            self.assertEqual(checkout.stat().st_uid, os.getuid())
            self.assertEqual(checkout.stat().st_mode & 0o777, 0o700)
            self.assertEqual(git('-C', str(checkout), 'rev-parse', 'HEAD').stdout.strip(), head)
            self.assertFalse(git('-C', str(checkout), 'status', '--porcelain').stdout)
            self.assertEqual((checkout / 'tracked.txt').read_bytes(), (source / 'tracked.txt').read_bytes())
            self.assertFalse((checkout / '.git/hooks/post-checkout').exists())
            self.assertEqual((source / '.git/config').read_bytes(), config_before)
            self.assertEqual((source / '.git/index').read_bytes(), index_before)
            self.assertNotEqual((source / 'tracked.txt').stat().st_ino, (checkout / 'tracked.txt').stat().st_ino)
            self.assertEqual((checkout / 'tracked.txt').stat().st_nlink, 1)
            local = git('-C', str(checkout), 'config', '--local', '--list').stdout
            self.assertNotIn('fixture-sensitive-header', local)
            self.assertNotIn('safe.directory', local)
            # Same cleared environment the adapter uses succeeds without trust.
            self.assertEqual(git('-C', str(checkout), 'show', 'HEAD:tracked.txt').stdout, 'exact public source\n')
            self.assertEqual(git('-C', str(source), '-c', 'safe.directory=' + str(source),
                                 'config', '--local', '--get', 'http.extraHeader').stdout.strip(), 'fixture-sensitive-header')

    def test_owned_group_drains_redirected_term_ignoring_descendant(self):
        # Controlled Python children only. The descendant closes every stream.
        for mode in ['success', 'error', 'timeout']:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory).resolve()
                p = object.__new__(assets.Producer)
                p.root, p.env = root, {'PATH': '/usr/bin:/bin', 'LANG': 'C'}
                p.counter, p.cancelled, p.child, p.groups = 0, False, None, {}
                pidfile = root / 'descendant.pid'
                descendant = ('import os,signal,time; from pathlib import Path; '
                              'signal.signal(signal.SIGTERM,signal.SIG_IGN); '
                              f'Path({str(pidfile)!r}).write_text(str(os.getpid())); '
                              'time.sleep(20)')
                leader = ('import subprocess,sys,time; from pathlib import Path; '
                          f'p=subprocess.Popen([sys.executable,"-c",{descendant!r}], '
                          'stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); '
                          f'path=Path({str(pidfile)!r}); '
                          '\nwhile not path.exists(): time.sleep(0.005)\n' +
                          ('time.sleep(20)' if mode == 'timeout' else
                           'raise SystemExit(42)' if mode == 'error' else 'raise SystemExit(0)'))
                try:
                    if mode == 'success':
                        p.call([sys.executable, '-c', leader], grace=0.05, timeout=0.5)
                    else:
                        with self.assertRaises((RuntimeError, subprocess.TimeoutExpired)):
                            p.call([sys.executable, '-c', leader], grace=0.05, timeout=0.5)
                    pid = int(pidfile.read_text())
                    with self.assertRaises(ProcessLookupError):
                        os.kill(pid, 0)
                    self.assertEqual(p.groups, {})
                    self.assertIsNone(p.child)
                finally:
                    # This exact test-owned PID/group is retained for RED cleanup.
                    if pidfile.exists():
                        try:
                            pgid = os.getpgid(int(pidfile.read_text()))
                            os.killpg(pgid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    if p.child is not None:
                        p.child.communicate(timeout=2)
                    if pidfile.exists():
                        deadline = time.monotonic() + 2
                        while True:
                            try:
                                os.kill(int(pidfile.read_text()), 0)
                            except ProcessLookupError:
                                break
                            self.assertLess(time.monotonic(), deadline, 'controlled descendant survived test cleanup')
                            time.sleep(0.01)

    def test_inaccessible_group_is_remaining_not_absent(self):
        p = object.__new__(assets.Producer)
        p.groups = {123: object()}
        with patch.object(os, 'killpg', side_effect=PermissionError):
            self.assertTrue(p.group_exists(123))
        with patch.object(os, 'killpg', side_effect=ProcessLookupError):
            self.assertFalse(p.group_exists(123))
        self.assertIn(123, p.groups)

    def test_failed_group_drain_retains_owner_and_blocks_cleanup(self):
        # A forced drain error tests retained ownership, without any signal call.
        with tempfile.TemporaryDirectory() as directory:
            p = object.__new__(assets.Producer)
            p.root, p.names = Path(directory), []
            child = type('RetainedChild', (), {'pid': 123})()
            p.groups, p.child = {123: child}, child
            with patch.object(p, 'drain_group', side_effect=RuntimeError('controlled drain failure')):
                with self.assertRaises(RuntimeError):
                    p.cleanup()
            self.assertIs(p.groups[123], child)
            self.assertIs(p.child, child)
            failed = json.loads((p.root / 'producer-cleanup.json').read_bytes())['failed']
            self.assertIn('process-group-123', failed)
            self.assertIn('process-groups', failed)

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
