import importlib.util
from contextlib import contextmanager
import hashlib
import io
import json
import os
import signal
import shutil
import struct
import time
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import tarfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
BUILD_CONFIG = b'{"os":"linux","architecture":"arm64","config":{"Tty":true,"StopTimeout":1,"Env":["A=1","B=2"]}}'
BUILD_CONFIG_ID = 'sha256:e1c93a3ed1766bed27c966f53476dd47fde4ae1f2cd16f933f3d989a55d0f789'
SPEC = importlib.util.spec_from_file_location('physical_assets', ROOT / '.github/scripts/mcp-stack-assets.py')
assets = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(assets)


class PhysicalGates(unittest.TestCase):
    @contextmanager
    def boot_fixture(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            p, source, git = self.source_fixture(root)
            git('update-index', '--add', '--cacheinfo', '160000,' + 'a' * 40 + ',third_party/firecracker')
            git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                '-c', 'commit.gpgsign=false', 'commit', '-m', 'fixture fork')
            p.backend, p.arch = 'firecracker', 'amd64'
            p.files = p.root / 'files'
            p.files.mkdir(mode=0o700)
            p.tools = {'gh': 'forbidden-gh', 'oras': 'forbidden-oras'}
            call = p.call
            def git_only(argv, **kwargs):
                self.assertEqual(argv[0], '/usr/bin/git', 'Boot input must not start a download or executable')
                return call(argv, **kwargs)
            p.call = git_only
            bundle = root / 'boot'
            bundle.mkdir(mode=0o700)
            ident = b'\x7fELF\x02\x01\x01' + bytes(9)
            elf = struct.pack('<16sHHIQQQIHHHHHH', ident, 2, 62, 1, 0x400000, 64, 0, 0, 64, 56, 1, 0, 0, 0)
            elf += struct.pack('<IIQQQQQQ', 1, 5, 0, 0x400000, 0, 120, 120, 4096)
            blobs = {'firecracker': elf, 'kernel': b'fixture kernel bytes',
                     'kernelChecksum': (hashlib.sha256(b'fixture kernel bytes').hexdigest() +
                                        '  vmlinux-engram-6.1.102-1\n').encode(),
                     'provenance': json.dumps({'version': 1, 'kind': 'operator-built-firecracker',
                                              'forkGitlink': 'a' * 40,
                                              'target': 'x86_64-unknown-linux-musl', 'profile': 'release',
                                              'firecrackerSha256': hashlib.sha256(elf).hexdigest()}).encode()}
            names = {'firecracker': 'firecracker', 'kernel': 'vmlinux-engram-6.1.102-1',
                     'kernelChecksum': 'vmlinux-engram-6.1.102-1.sha256', 'provenance': 'provenance.json'}
            manifest = {'version': 1, 'kind': 'mcp-physical-boot-assets', 'backend': 'firecracker',
                        'architecture': 'amd64', 'forkGitlink': 'a' * 40, 'kernelRelease': 'fc-kernel-6.1.102-1'}
            for key, data in blobs.items():
                path = bundle / names[key]
                path.write_bytes(data)
                path.chmod(0o400)
                manifest[key] = {'path': str(path), 'sha256': hashlib.sha256(data).hexdigest()}
            path = bundle / 'manifest.json'
            def save():
                if path.exists():
                    path.chmod(0o600)
                path.write_text(json.dumps(manifest))
                path.chmod(0o400)
            save()
            yield p, path, manifest, save

    def test_boot_artifacts_stage_exact_reviewed_bytes_without_download_or_execution(self):
        with self.boot_fixture() as (p, path, manifest, _):
            kernel, fork = p.acquire_boot(str(path))
            for key, staged in [('kernel', Path(kernel['path'])), ('firecracker', fork)]:
                self.assertEqual(hashlib.sha256(staged.read_bytes()).hexdigest(), manifest[key]['sha256'])
                self.assertNotEqual(staged.stat().st_ino, Path(manifest[key]['path']).stat().st_ino)
                self.assertTrue(staged.is_relative_to(p.root))
            receipt = json.loads((p.root / 'fork-provenance.json').read_bytes())
            self.assertEqual(receipt['kind'], 'operator-built')
            self.assertEqual(receipt['gitlink'], manifest['forkGitlink'])
            self.assertEqual(receipt['firecrackerSha256'], manifest['firecracker']['sha256'])
            self.assertNotIn('manifest', receipt)
            self.assertFalse((p.root / 'gh-config').exists())
            self.assertFalse((p.root / 'fork-registry.json').exists())

    def test_boot_artifacts_reject_invalid_manifest_and_never_download_fallback(self):
        cases = [('extra', True), ('version', True), ('kind', 'other'), ('backend', 'vz'),
                 ('architecture', 'arm64'), ('forkGitlink', 'b' * 40), ('kernelRelease', 'other'),
                 ('firecracker', {'path': '/invalid', 'sha256': 'z' * 64}),
                 ('kernel', {'path': '/invalid', 'sha256': 'a' * 64, 'extra': True})]
        for key, value in cases:
            with self.subTest(key=key), self.boot_fixture() as (p, path, manifest, save):
                manifest[key] = value
                save()
                with self.assertRaises(RuntimeError):
                    p.acquire_boot(str(path))
                self.assertFalse((p.root / 'gh-config').exists())

    def test_boot_artifacts_reject_duplicate_keys_in_manifest_pin_and_provenance(self):
        for where in ['manifest', 'pin', 'provenance']:
            with self.subTest(where=where), self.boot_fixture() as (p, path, manifest, save):
                if where == 'provenance':
                    receipt = Path(manifest['provenance']['path'])
                    receipt.chmod(0o600)
                    receipt.write_bytes(receipt.read_bytes().replace(b'"version": 1', b'"version": 1, "version": 1'))
                    receipt.chmod(0o400)
                    manifest['provenance']['sha256'] = hashlib.sha256(receipt.read_bytes()).hexdigest()
                    save()
                else:
                    path.chmod(0o600)
                    data = path.read_text()
                    data = data.replace('"version": 1', '"version": 1, "version": 1', 1) if where == 'manifest' else data.replace(
                        '"path": ', '"path": "/duplicate", "path": ', 1)
                    path.write_text(data)
                    path.chmod(0o400)
                with self.assertRaisesRegex(RuntimeError, 'boot_json_duplicate'):
                    p.acquire_boot(str(path))

    def test_boot_artifacts_reject_unreviewed_receipt_identity_and_unknown_fields(self):
        for key, value in [('version', True), ('kind', 'other'), ('forkGitlink', 'b' * 40),
                           ('target', 'aarch64-unknown-linux-musl'), ('profile', 'debug'),
                           ('firecrackerSha256', 'c' * 64), ('pass', True), ('command', 'arbitrary')]:
            with self.subTest(key=key), self.boot_fixture() as (p, path, manifest, save):
                receipt = Path(manifest['provenance']['path'])
                data = json.loads(receipt.read_bytes())
                data[key] = value
                receipt.chmod(0o600)
                receipt.write_text(json.dumps(data))
                receipt.chmod(0o400)
                manifest['provenance']['sha256'] = hashlib.sha256(receipt.read_bytes()).hexdigest()
                save()
                with self.assertRaisesRegex(RuntimeError, 'boot_provenance'):
                    p.acquire_boot(str(path))

    def test_boot_artifacts_reject_modified_pins_sidecar_and_nonstatic_elf(self):
        cases = ['fork_hash', 'kernel_hash', 'receipt_hash', 'checksum_hash', 'sidecar_value',
                 'sidecar_name', 'elf_arch', 'elf_class', 'elf_interp', 'elf_needed', 'elf_truncated']
        for case in cases:
            with self.subTest(case=case), self.boot_fixture() as (p, path, manifest, save):
                key = {'fork_hash': 'firecracker', 'kernel_hash': 'kernel', 'receipt_hash': 'provenance',
                       'checksum_hash': 'kernelChecksum'}.get(case, 'kernelChecksum' if case.startswith('sidecar') else 'firecracker')
                target = Path(manifest[key]['path'])
                data = bytearray(target.read_bytes())
                if case.endswith('_hash'):
                    data[-1] ^= 1
                elif case == 'sidecar_value':
                    data = bytearray(('b' * 64 + '  vmlinux-engram-6.1.102-1\n').encode())
                elif case == 'sidecar_name':
                    data = bytearray(data.replace(b'vmlinux-engram-', b'wrong-kernel-'))
                elif case == 'elf_arch':
                    struct.pack_into('<H', data, 18, 183)
                elif case == 'elf_class':
                    data[4] = 1
                elif case == 'elf_interp':
                    struct.pack_into('<I', data, 64, 3)
                elif case == 'elf_needed':
                    struct.pack_into('<H', data, 56, 2)
                    data += struct.pack('<IIQQQQQQ', 2, 4, 176, 0, 0, 16, 16, 8)
                    data += struct.pack('<qQ', 1, 0)
                else:
                    data = data[:80]
                target.chmod(0o600)
                target.write_bytes(data)
                target.chmod(0o400)
                if not case.endswith('_hash'):
                    manifest[key]['sha256'] = hashlib.sha256(data).hexdigest()
                    if key == 'firecracker':
                        receipt = Path(manifest['provenance']['path'])
                        provenance = json.loads(receipt.read_bytes())
                        provenance['firecrackerSha256'] = manifest[key]['sha256']
                        receipt.chmod(0o600)
                        receipt.write_text(json.dumps(provenance))
                        receipt.chmod(0o400)
                        manifest['provenance']['sha256'] = hashlib.sha256(receipt.read_bytes()).hexdigest()
                save()
                with self.assertRaises(RuntimeError):
                    p.acquire_boot(str(path))

    def test_boot_artifacts_reject_modes_links_nonprivate_parent_and_oversized_files(self):
        cases = ['manifest_writable', 'pin_writable', 'parent_public', 'symlink', 'hardlink',
                 'outside_bundle', 'fifo', 'oversized_manifest', 'oversized_fork', 'foreign_owner']
        for case in cases:
            with self.subTest(case=case), self.boot_fixture() as (p, path, manifest, save):
                target = Path(manifest['firecracker']['path'])
                if case == 'manifest_writable':
                    path.chmod(0o600)
                elif case == 'pin_writable':
                    target.chmod(0o600)
                elif case == 'parent_public':
                    path.parent.chmod(0o755)
                elif case == 'symlink':
                    target.rename(target.with_name('original'))
                    target.symlink_to(target.with_name('original'))
                elif case == 'hardlink':
                    os.link(target, target.with_name('alias'))
                elif case == 'outside_bundle':
                    destination = path.parent.parent / 'outside'
                    target.rename(destination)
                    manifest['firecracker']['path'] = str(destination)
                    save()
                elif case == 'fifo':
                    target.unlink()
                    os.mkfifo(target, 0o400)
                elif case.startswith('oversized'):
                    target = path if case == 'oversized_manifest' else target
                    target.chmod(0o600)
                    with target.open('r+b') as out:
                        out.truncate(16385 if case == 'oversized_manifest' else 256 * 1024 * 1024 + 1)
                    target.chmod(0o400)
                with patch.object(assets.os, 'getuid', return_value=os.getuid() + 1) if case == 'foreign_owner' else patch.dict(os.environ, {}):
                    with self.assertRaises((RuntimeError, OSError)):
                        p.acquire_boot(str(path))

    def test_boot_artifacts_reexec_carries_only_argument_and_no_artifact_token(self):
        with tempfile.TemporaryDirectory() as directory:
            p = object.__new__(assets.Producer)
            p.root, p.backend, p.operator_path = Path(directory), 'firecracker', '/owned/operator.json'
            p.env = {'PATH': '/usr/bin:/bin'}
            p.private_checkout = lambda source: (assets.REPO, 'a' * 40)
            calls = []
            def call(argv, **kwargs):
                calls.append((list(map(str, argv)), kwargs))
                nested = object.__new__(assets.Producer)
                nested.root, nested.backend, nested.head = p.root / 'execution', p.backend, 'a' * 40
                nested.root.mkdir()
                (nested.root / 'inputs.json').write_text('{"version":1}')
                nested.dispatch(prepare_only=True)
            p.call = call
            with patch.dict(os.environ, {'GITHUB_TOKEN': 'fixture-token', 'BOOT_ARTIFACTS': '/poison'}):
                p.run_private_checkout(prepare_only=True, boot_artifacts='/owned/boot/manifest.json')
            argv, kwargs = calls[0]
            self.assertIn('--boot-artifacts', argv)
            self.assertEqual(argv[argv.index('--boot-artifacts') + 1], '/owned/boot/manifest.json')
            self.assertEqual(kwargs['env'], p.env)

    def test_boot_artifacts_cli_rejects_vz_before_creating_work_root(self):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory) / 'work'
            result = subprocess.run([sys.executable, '-I', str(ROOT / '.github/scripts/mcp-stack-assets.py'),
                                     '--backend', 'vz', '--operator', '/absent/operator', '--work-root', str(work),
                                     '--boot-artifacts', '/absent/manifest'], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(work.exists())

    def test_boot_artifacts_reject_open_read_and_directory_replacement_races(self):
        for case in ['before_open', 'during_read', 'directory_replaced']:
            with self.subTest(case=case), self.boot_fixture() as (p, path, manifest, _):
                target = Path(manifest['firecracker']['path'])
                inode = target.stat().st_ino
                original_open, original_read = os.open, os.read
                changed = False
                def opening(name, flags, *args, **kwargs):
                    nonlocal changed
                    if case == 'before_open' and name == target.name and 'dir_fd' in kwargs:
                        target.rename(target.with_name('previous'))
                        target.write_bytes(target.with_name('previous').read_bytes())
                        target.chmod(0o400)
                        changed = True
                    return original_open(name, flags, *args, **kwargs)
                def reading(fd, size):
                    nonlocal changed
                    data = original_read(fd, size)
                    if case != 'before_open' and not changed and os.fstat(fd).st_ino == inode:
                        changed = True
                        if case == 'during_read':
                            target.chmod(0o600)
                            target.write_bytes(target.read_bytes())
                            target.chmod(0o400)
                        else:
                            previous = path.parent.with_name('previous-directory')
                            path.parent.rename(previous)
                            shutil.copytree(previous, path.parent)
                    return data
                with patch.object(assets.os, 'open', side_effect=opening), patch.object(assets.os, 'read', side_effect=reading):
                    with self.assertRaisesRegex(RuntimeError, 'boot_file_changed|boot_directory_changed'):
                        p.acquire_boot(str(path))
                self.assertTrue(changed)
                self.assertFalse((p.root / 'boot-download').exists())

    def test_boot_artifacts_reject_input_inside_source_context(self):
        with self.boot_fixture() as (p, path, _, _):
            p.source.mkdir()
            target = p.source / 'private-boot'
            path.parent.rename(target)
            with self.assertRaisesRegex(RuntimeError, 'boot_source_context'):
                p.acquire_boot(str(target / path.name))

    def test_boot_artifacts_static_check_bounds_dynamic_table_work(self):
        with self.boot_fixture() as (_, _, manifest, _):
            data = bytearray(Path(manifest['firecracker']['path']).read_bytes())
            struct.pack_into('<H', data, 56, 2)
            data += struct.pack('<IIQQQQQQ', 2, 4, 176, 0, 0, 65552, 65552, 8)
            data += bytes(65552)
            with self.assertRaisesRegex(RuntimeError, 'boot_fork_static'):
                assets.static_amd64_firecracker(data)

    def test_boot_artifacts_admission_fails_before_image_or_dependency_work(self):
        with self.native_sdk_fixture('firecracker') as (p, _, _, snapshot):
            snapshot.side_effect = None
            original_canonical = assets.canonical
            def device_path(path, *args, **kwargs):
                return Path('/dev/null') if path in {'/dev/kvm', '/dev/net/tun'} else original_canonical(path, *args, **kwargs)
            with patch.object(assets.os, 'getuid', return_value=0), \
                 patch.object(assets, 'canonical', side_effect=device_path), \
                 patch.object(p, 'acquire_boot', side_effect=RuntimeError('invalid_boot_bundle')) as acquire, \
                 patch.object(p, 'archive') as archive, patch.object(p, 'skopeo') as skopeo:
                with self.assertRaisesRegex(RuntimeError, 'invalid_boot_bundle'):
                    p.produce(boot_artifacts='/private/manifest.json')
            acquire.assert_called_once_with('/private/manifest.json')
            archive.assert_not_called()
            skopeo.assert_not_called()

    def test_boot_artifacts_absent_keeps_fixed_fc_registry_and_vz_kernel_paths(self):
        for backend in ['firecracker', 'vz']:
            with self.subTest(backend=backend), self.boot_fixture() as (p, _, _, _):
                p.backend, p.arch = backend, 'amd64' if backend == 'firecracker' else 'arm64'
                call, commands = p.call, []
                def download(argv, **kwargs):
                    commands.append((list(map(str, argv)), kwargs))
                    if argv[0] == '/usr/bin/git':
                        return call(argv, **kwargs)
                    if argv[0] == 'forbidden-gh':
                        target = Path(argv[-1])
                        target.write_bytes((hashlib.sha256(b'kernel').hexdigest() + '\n').encode()
                                           if str(target).endswith('.sha256') else b'kernel')
                    elif argv[1] == 'resolve':
                        return ('sha256:' + 'b' * 64).encode()
                    elif argv[1] == 'pull':
                        target = Path(argv[-1])
                        target.mkdir()
                        (target / 'firecracker').write_bytes(b'registry fork bytes')
                    return b''
                p.call = download
                with patch.dict(os.environ, {'GITHUB_TOKEN': 'fixture-token'}):
                    kernel, fork = p.acquire_boot()
                self.assertEqual(Path(kernel['path']).read_bytes(), b'kernel')
                gh = [argv for argv, _ in commands if argv[0] == 'forbidden-gh']
                self.assertEqual(len(gh), 2)
                self.assertIn('vmlinux-engram-6.1.102-1' if backend == 'firecracker' else 'Image-engram-6.1.102-1', gh[0])
                oras = [argv for argv, _ in commands if argv[0] == 'forbidden-oras']
                if backend == 'firecracker':
                    self.assertIn('ghcr.io/cortexapps/engrams/firecracker:' + 'a' * 40, oras[1])
                    self.assertIn('ghcr.io/cortexapps/engrams/firecracker@sha256:' + 'b' * 64, oras[2])
                    self.assertEqual(fork.read_bytes(), b'registry fork bytes')
                    self.assertEqual(json.loads((p.root / 'fork-provenance.json').read_bytes()),
                                     {'gitlink': 'a' * 40, 'manifest': 'sha256:' + 'b' * 64})
                else:
                    self.assertIsNone(fork)
                    self.assertEqual(oras, [])

    def test_guard_apt_retries_are_bounded_and_failed_update_blocks_install(self):
        source = (ROOT / 'tests/fixtures/mcp-stack/platform/guard.Dockerfile').read_text()
        commands = source.split('> /etc/apt/sources.list; \\\n', 1)[1].split(
            '    rm -rf /var/lib/apt/lists/', 1)[0].replace('\\\n', '')
        shell = 'set -eu;\n' + commands
        subprocess.run(['/bin/sh', '-n'], input=shell, text=True, check=True)
        cases = [(0, 0, 0, 1, 1), (2, 1, 0, 3, 2), (3, 3, 0, 4, 4),
                 (4, 0, 1, 4, 0), (0, 4, 1, 1, 4)]
        for update_failures, install_failures, failed, updates, installs in cases:
            with self.subTest(update_failures=update_failures, install_failures=install_failures):
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    log = root / 'calls.json'
                    stub = root / 'apt-get'
                    stub.write_text(f'#!{sys.executable} -I\n' +
                                    'import json, os, sys\n'
                                    'from pathlib import Path\n'
                                    'log = Path(os.environ["APT_STUB_LOG"])\n'
                                    'calls = json.loads(log.read_text()) if log.exists() else []\n'
                                    'args = sys.argv[1:]\n'
                                    'operation = "update" if "update" in args else "install"\n'
                                    'calls.append({"operation": operation, "args": args})\n'
                                    'log.write_text(json.dumps(calls))\n'
                                    'attempt = sum(c["operation"] == operation for c in calls)\n'
                                    'sys.exit(100 if attempt <= int(os.environ[operation.upper() + "_FAILURES"]) else 0)\n')
                    stub.chmod(0o700)
                    result = subprocess.run(['/bin/sh', '-c', shell], capture_output=True,
                                            text=True, timeout=5, env={
                                                'PATH': directory, 'APT_STUB_LOG': str(log),
                                                'UPDATE_FAILURES': str(update_failures),
                                                'INSTALL_FAILURES': str(install_failures),
                                            })
                    self.assertEqual(result.returncode != 0, bool(failed), result.stderr)
                    calls = json.loads(log.read_text())
                    self.assertEqual([call['operation'] for call in calls],
                                     ['update'] * updates + ['install'] * installs)
                    for call in calls:
                        args = call['args']
                        self.assertEqual(args[:4], ['-o', 'Acquire::Retries=0',
                                                   '-o', 'Acquire::http::Timeout=30'])
                        if call['operation'] == 'update':
                            self.assertEqual(args[4:], ['--error-on=any', 'update'])

    def test_thin_pool_operator_rejects_unknown_keys_names_paths_and_modes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            valid = {'name': 'suite', 'directory': str(root)}
            self.assertEqual(assets.validate_thin_pool(valid), valid)
            for value in [None, {}, {**valid, 'extra': True}, {**valid, 'name': 'a' * 128},
                          {**valid, 'name': '../pool'}, {**valid, 'name': 'pool;true'},
                          {**valid, 'directory': str(root) + '/..'}]:
                with self.subTest(value=value), self.assertRaises((RuntimeError, TypeError)):
                    assets.validate_thin_pool(value)
            root.chmod(0o755)
            with self.assertRaisesRegex(RuntimeError, 'operator_thin_pool_mode'):
                assets.validate_thin_pool(valid)
            root.chmod(0o700)
            with patch.object(assets.os, 'getuid', return_value=os.getuid() + 1):
                with self.assertRaisesRegex(RuntimeError, 'path_owner'):
                    assets.validate_thin_pool(valid)

    def test_source_context_rejects_secrets(self):
        assets.source_paths(['docker/host-agent.Dockerfile', '.env.example', 'orchestrator/src/index.ts'])
        for path in ['.env', 'orchestrator/.env.local', 'var/key', '.git/config', '.superpowers/private',
                     'crates/engram-coordinator/var/key', '../key', '/tmp/key']:
            with self.subTest(path=path), self.assertRaises(RuntimeError):
                assets.source_paths([path])

    def source_fixture(self, root, extra=()):
        source, work = root / 'fixture', root / 'work'
        source.mkdir()
        work.mkdir(mode=0o700)
        env = {'PATH': '/usr/bin:/bin', 'LANG': 'C', 'GIT_CONFIG_NOSYSTEM': '1',
               'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_SYSTEM': '/dev/null'}
        def git(*args):
            return subprocess.run(['/usr/bin/git', '-C', str(source), *args], env=env,
                                  capture_output=True, check=True)
        git('init', '--template=')
        recipes = ['docker/coordinator.Dockerfile', 'docker/orchestrator.Dockerfile',
                   'docker/mcp-worker.Dockerfile', 'deploy/demo/Dockerfile',
                   '.github/scripts/mcp-stack-inputs.mts', '.github/scripts/mcp-stack-fc.sh',
                   '.github/scripts/mcp-stack-vz.sh', 'tests/fixtures/mcp-stack/platform/main.ts',
                   'tests/fixtures/mcp-stack/platform/fixture.Dockerfile',
                   'tests/fixtures/mcp-stack/platform/guest.Dockerfile',
                   'tests/fixtures/mcp-stack/platform/guard.Dockerfile',
                   'tests/fixtures/mcp-stack/platform/host-agent.Dockerfile']
        for name in [*recipes, 'tracked.txt', *extra]:
            path = source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('public fixture source\n')
        git('add', '.')
        git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
            '-c', 'commit.gpgsign=false', 'commit', '-m', 'fixture')
        p = object.__new__(assets.Producer)
        p.root, p.source, p.env = work, work / 'source', env
        p.counter, p.cancelled, p.child, p.groups = 0, False, None, {}
        call = p.call
        p.call = lambda argv, **kwargs: call(argv, cwd=source, **kwargs)
        return p, source, git

    def test_snapshot_excludes_only_known_legacy_runtime_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            legacy = 'crates/engram-coordinator/var/snapshots/fixture.bin'
            safe = ['literal[1].txt', 'line\nbreak.txt',
                    'crates/engram-coordinator/var-source/fixture.rs']
            p, source, git = self.source_fixture(Path(directory), [legacy, *safe])
            (source / 'third_party/fixture').mkdir(parents=True)
            git('update-index', '--add', '--cacheinfo',
                '160000,' + git('rev-parse', 'HEAD').stdout.decode().strip() + ',third_party/fixture')
            git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                '-c', 'commit.gpgsign=false', 'commit', '-m', 'fixture gitlink')
            (source / '.env').write_text('untracked fixture secret\n')
            archives = []
            call = p.call
            def capture(argv, **kwargs):
                result = call(argv, **kwargs)
                if 'archive' in argv:
                    archives.append(result)
                return result
            p.call = capture
            p.snapshot()
            self.assertEqual(p.head, git('rev-parse', 'HEAD').stdout.decode().strip())
            self.assertEqual((p.source / 'tracked.txt').read_text(), 'public fixture source\n')
            for name in safe:
                self.assertTrue((p.source / name).is_file())
            self.assertFalse((p.source / 'crates/engram-coordinator/var').exists())
            self.assertFalse((p.source / '.env').exists())
            self.assertTrue((p.source / 'third_party/fixture').is_dir())
            self.assertTrue((source / legacy).is_file())
            self.assertEqual(len(archives), 1)
            with tarfile.open(fileobj=io.BytesIO(archives[0])) as tar:
                members = {m.name for m in tar.getmembers()}
            self.assertIn('tracked.txt', members)
            self.assertNotIn(legacy, members)
            self.assertNotIn('crates/engram-coordinator/var', members)

    def test_snapshot_rejects_other_forbidden_tracked_paths(self):
        for name in ['var/key', 'crates/other/var/key', '.env', 'orchestrator/.env.local',
                     '.superpowers/private', 'crates/engram-coordinator/var-old/var/key']:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                p, _, _ = self.source_fixture(Path(directory), [name])
                with self.assertRaisesRegex(RuntimeError, 'secret_source_context'):
                    p.snapshot()
                self.assertFalse(p.source.exists())

    def test_snapshot_preserves_public_source_modes_under_private_umask(self):
        with tempfile.TemporaryDirectory() as directory:
            files = ['orchestrator/src/db/migrate.ts', 'orchestrator/drizzle/meta/_journal.json',
                     'scripts/run.sh']
            p, source, git = self.source_fixture(Path(directory), files)
            (source / 'scripts/run.sh').chmod(0o755)
            git('update-index', '--chmod=+x', 'scripts/run.sh')
            git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                '-c', 'commit.gpgsign=false', 'commit', '-m', 'fixture executable')
            (source / '.env').write_text('untracked fixture secret\n')
            previous_umask = os.umask(0o077)
            try:
                p.snapshot()
            finally:
                os.umask(previous_umask)
            for path in [p.root, p.source]:
                self.assertEqual(path.stat().st_mode & 0o7777, 0o700)
            for name in ['orchestrator', 'orchestrator/src', 'orchestrator/src/db',
                         'orchestrator/drizzle', 'orchestrator/drizzle/meta', 'scripts']:
                with self.subTest(directory=name):
                    self.assertEqual((p.source / name).stat().st_mode & 0o7777, 0o755)
            for name, mode in [('tracked.txt', 0o644), (files[0], 0o644),
                               (files[1], 0o644), (files[2], 0o755)]:
                with self.subTest(file=name):
                    self.assertEqual((p.source / name).stat().st_mode & 0o7777, mode)
                    self.assertEqual((p.source / name).read_bytes(), b'public fixture source\n')
            self.assertFalse((p.source / '.env').exists())

    def test_snapshot_checks_all_archive_members_before_extraction(self):
        for name, kind in [('unexpected.txt', tarfile.REGTYPE), ('var/key', tarfile.REGTYPE),
                           ('.env', tarfile.REGTYPE), ('.git/config', tarfile.REGTYPE),
                           ('.superpowers/private', tarfile.REGTYPE),
                           ('crates/engram-coordinator/var/key', tarfile.REGTYPE),
                           ('../escape', tarfile.REGTYPE), ('/tmp/escape', tarfile.REGTYPE),
                           ('safe/../../escape', tarfile.REGTYPE), ('./tracked.txt', tarfile.REGTYPE),
                           ('tracked.txt', tarfile.REGTYPE), ('unexpected', tarfile.DIRTYPE),
                           ('tracked.txt', tarfile.SYMTYPE), ('tracked.txt', tarfile.LNKTYPE),
                           ('tracked.txt', tarfile.FIFOTYPE), (None, None)]:
            with self.subTest(name=name, kind=kind), tempfile.TemporaryDirectory() as directory:
                p, _, _ = self.source_fixture(Path(directory))
                call = p.call
                def corrupt(argv, **kwargs):
                    result = call(argv, **kwargs)
                    if 'archive' not in argv:
                        return result
                    output = io.BytesIO()
                    with (tarfile.open(fileobj=io.BytesIO(result)) as original,
                          tarfile.open(fileobj=output, mode='w') as tar):
                        for member in original.getmembers():
                            if member.name == 'tracked.txt' and (name is None or kind != tarfile.REGTYPE):
                                continue
                            tar.addfile(member, original.extractfile(member) if member.isfile() else None)
                        if name is not None:
                            member = tarfile.TarInfo(name)
                            member.type = kind
                            if kind in {tarfile.SYMTYPE, tarfile.LNKTYPE}:
                                member.linkname = 'tracked.txt'
                            tar.addfile(member)
                    return output.getvalue()
                p.call = corrupt
                with self.assertRaises(RuntimeError):
                    p.snapshot()
                self.assertFalse((p.source / 'tracked.txt').exists())
                self.assertFalse((p.source / 'docker').exists())

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

    @contextmanager
    def archive_fixture(self):
        # Only controlled executables run. No Docker daemon or image is used.
        with tempfile.TemporaryDirectory() as directory, socket.socket(socket.AF_UNIX) as sock:
            root = Path(directory).resolve()
            sock.bind(str(root / 'engine.sock'))
            script = f'''#!{sys.executable}
import hashlib, json, os, sys
from pathlib import Path
root = Path({str(root)!r})
kind, args = Path(sys.argv[0]).name, sys.argv[1:]
with (root / 'calls').open('a') as out:
    out.write(json.dumps([kind, *args]) + '\\n')
operation = (args[4:6] if kind == 'docker' else
             [kind, 'inspect-' + ('docker' if args[-1].startswith('docker-archive:') else 'oci')
              if 'inspect' in args else 'copy'])
if (root / 'fail').exists() and operation == json.loads((root / 'fail').read_text()):
    raise SystemExit(42)
def emit(path, data, stage):
    mode = (root / (stage + '-kind')).read_text() if (root / (stage + '-kind')).exists() else 'normal'
    if mode in ['symlink', 'hardlink']:
        target = root / (stage + '-target')
        target.write_bytes(data)
        path.symlink_to(target) if mode == 'symlink' else os.link(target, path)
    else:
        path.write_bytes(b'' if mode == 'empty' else data)
if kind == 'docker':
    assert not {{'HOME', 'DOCKER_HOST', 'DOCKER_CONTEXT', 'GITHUB_TOKEN', 'REGISTRY_AUTH_FILE'}}.intersection(os.environ)
    assert args[:4] == ['--config', str(root / 'producer-engine-config'), '--host', 'unix://' + str(root / 'engine.sock')]
    args = args[4:]
    if args[0] == 'info':
        print(json.dumps({{'ID': 'controlled-engine-identity'}}))
    elif args[:2] == ['buildx', 'build']:
        assert args[2:4] == ['--builder', 'default']
        if '--load' in args:
            path = root / 'metadata'
            if path.exists():
                emit(Path(args[args.index('--metadata-file') + 1]), path.read_bytes(), 'metadata')
        else:
            output = args[args.index('--output') + 1]
            assert output.startswith('type=local,dest=')
            Path(output.split('dest=', 1)[1]).mkdir()
    elif args[:2] == ['image', 'inspect']:
        actual = json.loads((root / 'inspect').read_bytes())
        identity = {BUILD_CONFIG_ID!r}
        if (root / 'loaded').exists():
            identity = 'sha256:' + hashlib.sha256((root / 'config-bytes').read_bytes()).hexdigest()
            actual[0]['Id'] = identity
        assert args[2:] == [identity]
        print(json.dumps(actual))
    elif args[:2] == ['image', 'save']:
        assert args[2] == '--output' and args[4:] == [{BUILD_CONFIG_ID!r}]
        emit(Path(args[3]), b'controlled-docker-archive', 'save')
    elif args[0] == 'load':
        assert args[1] == '--input' and Path(args[2]).read_bytes() == b'controlled-docker-load'
        (root / 'loaded').touch()
    else:
        raise AssertionError(args)
else:
    assert kind == 'skopeo'
    if 'inspect' in args:
        assert '--raw' in args
        if args[-1].startswith('docker-archive:'):
            assert '--config' in args
            sys.stdout.buffer.write((root / 'saved-config-bytes').read_bytes())
        elif '--config' in args:
            sys.stdout.buffer.write((root / 'config-bytes').read_bytes())
        else:
            config = (root / 'config-bytes').read_bytes()
            print(json.dumps({{'schemaVersion': 2, 'config': {{'digest': 'sha256:' + hashlib.sha256(config).hexdigest()}}}}))
    else:
        assert 'copy' in args
        if args[-2].startswith('docker-archive:'):
            assert Path(args[-2].removeprefix('docker-archive:')).read_bytes() == b'controlled-docker-archive'
            Path(args[-1].removeprefix('oci-archive:')).write_bytes(b'controlled-oci-archive')
        else:
            assert Path(args[-2].removeprefix('oci-archive:')).read_bytes() == b'controlled-oci-archive'
            Path(args[-1].removeprefix('docker-archive:')).write_bytes(b'controlled-docker-load')
'''
            tools = {}
            for name in ['docker', 'skopeo']:
                path = root / name
                path.write_text(script)
                path.chmod(0o700)
                tools[name] = str(path)
            p = assets.Producer('vz', root, {'engineSocket': str(root / 'engine.sock'), 'tools': tools})
            p.engine_identity, p.head = 'controlled-engine-identity', 'b' * 40
            (root / 'config-bytes').write_bytes(BUILD_CONFIG)
            (root / 'saved-config-bytes').write_bytes(BUILD_CONFIG)
            (root / 'metadata').write_text(json.dumps({
                'containerimage.config.digest': BUILD_CONFIG_ID,
                'containerimage.digest': 'sha256:' + 'c' * 64}))
            actual = {'Id': BUILD_CONFIG_ID, 'Os': 'linux', 'Architecture': 'arm64',
                      'Config': {'Labels': {'org.opencontainers.image.revision': p.head, assets.LABEL: p.run}}}
            (root / 'inspect').write_text(json.dumps([actual]))
            yield p, actual

    def test_archive_loads_then_saves_exact_config_before_oci_conversion(self):
        with self.archive_fixture() as (p, _):
            context = p.root / 'custom-context'
            archive = p.archive('hostAgent', 'Dockerfile', context, {'HOST_IMAGE': 'controlled-base'})
            self.assertEqual(archive.read_bytes(), b'controlled-oci-archive')
            calls = [json.loads(line) for line in (p.root / 'calls').read_text().splitlines()]
            operations = [call[5:] for call in calls if call[0] == 'docker' and call[5] != 'info']
            self.assertEqual([call[:2] for call in operations],
                             [['buildx', 'build'], ['image', 'inspect'], ['image', 'save']])
            build, inspect, save = operations
            self.assertIn('--load', build)
            self.assertNotIn('--output', build)
            self.assertNotIn('--push', build)
            self.assertIn('--provenance=false', build)
            self.assertIn('--sbom=false', build)
            self.assertIn('org.opencontainers.image.revision=' + p.head, build)
            self.assertIn(assets.LABEL + '=' + p.run, build)
            self.assertIn('SOURCE_COMMIT=' + p.head, build)
            self.assertEqual(build[build.index('--platform') + 1], 'linux/arm64')
            metadata = Path(build[build.index('--metadata-file') + 1])
            self.assertEqual(metadata.parent, p.root)
            self.assertEqual(metadata.read_bytes(), (p.root / 'metadata').read_bytes())
            self.assertEqual(build[build.index('--file') + 1], str(context / 'Dockerfile'))
            self.assertIn('HOST_IMAGE=controlled-base', build)
            self.assertEqual(build[-1], str(context))
            self.assertEqual(inspect[2:], [BUILD_CONFIG_ID])
            self.assertEqual(save, ['image', 'save', '--output', str(p.files / 'hostAgent.docker.tar'),
                                    BUILD_CONFIG_ID])
            self.assertEqual(calls[-1][0], 'skopeo')
            for call in operations:
                self.assertNotIn('--tag', call)
                self.assertNotIn('-t', call)
                self.assertNotIn('--iidfile', call)
            self.assertEqual(p.names, [])

    def test_guest_archive_creates_private_state_before_start_and_keeps_existing_proof(self):
        with self.archive_fixture() as (p, _):
            demo = 'deploy/demo/Dockerfile'
            selected = assets.SOURCE_IMAGES['guest']
            for name in {demo, selected}:
                target = p.source / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes((ROOT / name).read_bytes())
            p.archive('guest', selected)
            calls = [json.loads(line) for line in (p.root / 'calls').read_text().splitlines()]
            build = next(call for call in calls if call[0] == 'docker' and call[5:7] == ['buildx', 'build'])
            recipe = Path(build[build.index('--file') + 1]).read_bytes()
            self.assertTrue(recipe.startswith((ROOT / demo).read_bytes()))
            self.assertEqual(recipe, (ROOT / demo).read_bytes() + b'\n' + (ROOT / selected).read_bytes())
            self.assertEqual(build[-1], str(p.source))
            self.assertIn('SOURCE_COMMIT=' + p.head, build)
            self.assertIn('org.opencontainers.image.revision=' + p.head, build)
            self.assertIn(assets.LABEL + '=' + p.run, build)
            instruction = recipe.decode().strip().splitlines()[-1]
            self.assertTrue(instruction.startswith('RUN '))
            state = p.root / 'state'
            command = instruction.removeprefix('RUN ').replace('/tmp/mcp-stack-fixture', str(state))
            command = command.replace('/workspace/.engrams', str(p.root / 'workspace'))
            def setup():
                subprocess.run(['/bin/sh', '-c', 'umask 022; ' + command], check=True)
            setup()
            self.assertTrue(state.is_dir())
            self.assertFalse(state.is_symlink())
            self.assertEqual(state.stat().st_mode & 0o777, 0o700)
            self.assertEqual(state.stat().st_uid, os.getuid())
            proof = state / 'proof.jsonl'
            proof.write_bytes(b'existing proof\n')
            setup()
            self.assertEqual(proof.read_bytes(), b'existing proof\n')
            self.assertEqual(state.stat().st_mode & 0o777, 0o700)
            self.assertEqual(state.stat().st_uid, os.getuid())

    def test_archive_rejects_missing_or_invalid_config_metadata_before_inspect(self):
        for metadata in [None, b'not-json', b'[]', b'{}',
                         json.dumps({'containerimage.digest': BUILD_CONFIG_ID}).encode(),
                         *[json.dumps({'containerimage.config.digest': value}).encode()
                           for value in [None, 7, 'a' * 64, 'sha256:' + 'A' * 64,
                                         'sha256:' + 'a' * 63, BUILD_CONFIG_ID + '\n']]]:
            with self.subTest(metadata=metadata), self.archive_fixture() as (p, _):
                if metadata is None:
                    (p.root / 'metadata').unlink()
                else:
                    (p.root / 'metadata').write_bytes(metadata)
                with self.assertRaises((RuntimeError, ValueError, FileNotFoundError)) as failure:
                    p.archive('worker', 'docker/mcp-worker.Dockerfile')
                if isinstance(failure.exception, RuntimeError):
                    self.assertEqual(str(failure.exception), 'build_image_config_digest')
                calls = [json.loads(line) for line in (p.root / 'calls').read_text().splitlines()]
                self.assertFalse(any(call[0] == 'skopeo' or call[5:7] in
                                     [['image', 'inspect'], ['image', 'save']] for call in calls))
                self.assertFalse((p.files / 'worker.docker.tar').exists())

    def test_archive_rejects_inspect_mismatch_before_save_or_conversion(self):
        for field, value in [('Id', 'sha256:' + 'c' * 64), ('Os', 'darwin'), ('Architecture', 'amd64'),
                             ('revision', 'c' * 40), ('run', 'foreign'), ('Config', {}),
                             ('Labels', None)]:
            with self.subTest(field=field), self.archive_fixture() as (p, actual):
                if field in ['revision', 'run']:
                    key = 'org.opencontainers.image.revision' if field == 'revision' else assets.LABEL
                    actual['Config']['Labels'][key] = value
                elif field == 'Labels':
                    actual['Config']['Labels'] = value
                else:
                    actual[field] = value
                (p.root / 'inspect').write_text(json.dumps([actual]))
                seam = 'build_image_platform' if field in ['Id', 'Os', 'Architecture'] else 'build_image_labels'
                with self.assertRaisesRegex(RuntimeError, seam):
                    p.archive('worker', 'docker/mcp-worker.Dockerfile')
                calls = [json.loads(line) for line in (p.root / 'calls').read_text().splitlines()]
                self.assertFalse(any(call[0] == 'skopeo' or call[5:7] == ['image', 'save'] for call in calls))
                self.assertFalse((p.files / 'worker.docker.tar').exists())

    def test_archive_local_directory_output_keeps_default_driver_path(self):
        with self.archive_fixture() as (p, _):
            output = p.archive('linux-assets', assets.CI + '/linux-assets.Dockerfile', local=True)
            self.assertEqual(output, p.files / 'linux-assets')
            self.assertTrue(output.is_dir())
            calls = [json.loads(line) for line in (p.root / 'calls').read_text().splitlines()]
            builds = [call[5:] for call in calls if call[0] == 'docker' and call[5] != 'info']
            self.assertEqual(len(builds), 1)
            build = builds[0]
            self.assertEqual(build[build.index('--output') + 1], 'type=local,dest=' + str(output))
            self.assertNotIn('--load', build)
            self.assertNotIn('--metadata-file', build)
            self.assertFalse(any(call[0] == 'skopeo' for call in calls))

    def test_archive_requires_one_inspected_image(self):
        for response in [[], [{}, {}]]:
            with self.subTest(response=response), self.archive_fixture() as (p, actual):
                if response:
                    response = [actual, actual]
                (p.root / 'inspect').write_text(json.dumps(response))
                with self.assertRaisesRegex(RuntimeError, 'build_image_inspect'):
                    p.archive('worker', 'docker/mcp-worker.Dockerfile')
                self.assertFalse((p.files / 'worker.docker.tar').exists())

    def test_archive_requires_owned_regular_metadata_and_saved_file(self):
        for stage in ['metadata', 'save']:
            for kind in ['symlink', 'hardlink', 'empty']:
                with self.subTest(stage=stage, kind=kind), self.archive_fixture() as (p, _):
                    (p.root / (stage + '-kind')).write_text(kind)
                    with self.assertRaises(RuntimeError):
                        p.archive('worker', 'docker/mcp-worker.Dockerfile')
                    calls = [json.loads(line) for line in (p.root / 'calls').read_text().splitlines()]
                    self.assertFalse(any(call[0] == 'skopeo' for call in calls))
                    if stage == 'metadata':
                        self.assertFalse((p.files / 'worker.docker.tar').exists())

    def test_archive_rejects_converted_config_mismatch(self):
        with self.archive_fixture() as (p, _):
            (p.root / 'config-bytes').write_bytes(b'{"os":"foreign"}')
            with self.assertRaisesRegex(RuntimeError, 'build_archive_config'):
                p.archive('worker', 'docker/mcp-worker.Dockerfile')

    def test_archive_saved_config_digest_must_match_before_conversion(self):
        with self.archive_fixture() as (p, _):
            # The same values in a different byte representation do not have the build ID.
            (p.root / 'saved-config-bytes').write_bytes(json.dumps(json.loads(BUILD_CONFIG)).encode())
            with self.assertRaisesRegex(RuntimeError, 'build_saved_config'):
                p.archive('worker', 'docker/mcp-worker.Dockerfile')
            calls = [json.loads(line) for line in (p.root / 'calls').read_text().splitlines()]
            self.assertFalse(any(call[0] == 'skopeo' and 'copy' in call for call in calls))
            self.assertFalse((p.files / 'worker.oci.tar').exists())

    def test_archive_accepts_object_key_order_and_imports_actual_converted_id(self):
        with self.archive_fixture() as (p, _):
            converted = json.dumps(json.loads(BUILD_CONFIG), sort_keys=True, separators=(',', ':')).encode()
            self.assertNotEqual(converted, BUILD_CONFIG)
            converted_id = 'sha256:' + hashlib.sha256(converted).hexdigest()
            self.assertNotEqual(converted_id, BUILD_CONFIG_ID)
            (p.root / 'config-bytes').write_bytes(converted)
            archive = p.archive('worker', 'docker/mcp-worker.Dockerfile')
            facts = p.import_image('worker', archive)
            self.assertEqual(facts['id'], converted_id)
            self.assertEqual(p.images['worker']['id'], converted_id)
            calls = [json.loads(line) for line in (p.root / 'calls').read_text().splitlines()]
            inspections = [call[7:] for call in calls if call[0] == 'docker' and call[5:7] == ['image', 'inspect']]
            self.assertEqual(inspections, [[BUILD_CONFIG_ID], [converted_id]])

    def test_archive_rejects_config_value_type_or_list_order_changes(self):
        for key, value in [('Tty', 1), ('StopTimeout', True), ('StopTimeout', 1.0),
                           ('StopTimeout', '1'), ('Env', ['B=2', 'A=1']), ('Env', 'A=1'),
                           ('NewField', None), ('StopTimeout', float('nan'))]:
            with self.subTest(key=key, value=value), self.archive_fixture() as (p, _):
                config = json.loads(BUILD_CONFIG)
                config['config'][key] = value
                (p.root / 'config-bytes').write_text(json.dumps(config))
                with self.assertRaises((RuntimeError, ValueError)):
                    p.archive('worker', 'docker/mcp-worker.Dockerfile')

    def test_archive_command_errors_stop_without_fallback(self):
        operations = [['buildx', 'build'], ['image', 'inspect'], ['image', 'save'],
                      ['skopeo', 'inspect-docker'], ['skopeo', 'copy'], ['skopeo', 'inspect-oci']]
        for index, operation in enumerate(operations):
            with self.subTest(operation=operation), self.archive_fixture() as (p, _):
                (p.root / 'fail').write_text(json.dumps(operation))
                with self.assertRaisesRegex(RuntimeError, 'producer_command'):
                    p.archive('worker', 'docker/mcp-worker.Dockerfile')
                calls = [json.loads(line) for line in (p.root / 'calls').read_text().splitlines()]
                actual = [call[5:7] if call[0] == 'docker' else
                          ['skopeo', 'inspect-' + ('docker' if call[-1].startswith('docker-archive:') else 'oci')
                           if 'inspect' in call else 'copy'] for call in calls
                          if call[0] != 'docker' or call[5] != 'info']
                self.assertEqual(actual, operations[:index + 1])

    def test_prepare_dispatch_pins_inputs_without_adapter_or_matrix(self):
        with tempfile.TemporaryDirectory() as directory:
            p = object.__new__(assets.Producer)
            p.root, p.backend, p.head = Path(directory), 'vz', 'a' * 40
            p.env, p.files = {}, p.root
            path = p.root / 'inputs.json'
            path.write_text('{"version":1}')
            def forbidden(*args, **kwargs):
                self.fail('preparation must not dispatch an adapter or inspect a matrix')
            p.call = forbidden
            with patch.object(assets, 'matrix_result', forbidden):
                self.assertEqual(p.dispatch(prepare_only=True), 0)
            receipt = json.loads((p.root / 'assets-prepared.json').read_bytes())
            self.assertEqual(receipt['kind'], 'assets-prepared')
            self.assertFalse(receipt['physicalAcceptance'])
            self.assertFalse(receipt['matrixCompleted'])
            self.assertEqual(receipt['inputs']['sha256'], hashlib.sha256(path.read_bytes()).hexdigest())
            self.assertEqual(path.stat().st_mode & 0o777, 0o400)
            self.assertFalse(getattr(p, 'adapter_started', False))
            self.assertEqual(assets.prepared_result(p.root, 'vz', p.head), 0)
            path.chmod(0o600)
            path.write_text('changed')
            with self.assertRaises(RuntimeError):
                assets.prepared_result(p.root, 'vz', p.head)

    def test_private_preparation_propagates_mode_and_verifies_nested_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            p = object.__new__(assets.Producer)
            p.root, p.backend, p.operator_path = Path(directory), 'vz', '/owned/operator.json'
            p.env = {'PATH': '/usr/bin:/bin'}
            p.private_checkout = lambda source: (assets.REPO, 'a' * 40)
            calls = []
            def call(argv, **kwargs):
                calls.append((list(map(str, argv)), kwargs))
                nested = object.__new__(assets.Producer)
                nested.root, nested.backend, nested.head = p.root / 'execution', p.backend, 'a' * 40
                nested.root.mkdir()
                (nested.root / 'inputs.json').write_text('{"version":1}')
                nested.dispatch(prepare_only=True)
            p.call = call
            self.assertEqual(p.run_private_checkout(prepare_only=True), 0)
            argv, kwargs = calls[0]
            self.assertEqual(argv[-2:], ['--private-checkout', '--prepare-only'])
            self.assertEqual(kwargs['grace'], 240)
            self.assertTrue(p.nested_started)
            self.assertFalse((p.root / 'execution/run').exists())

    def test_prepared_and_filtered_receipts_cannot_satisfy_matrix_gate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'completion-1.json').write_text(json.dumps({
                'backend':'vz','sourceCommit':'a' * 40,'matrixCompleted':True,
                'matrix':'all','physicalAcceptance':True,'stageCompleted':False,'cleanupFailed':[]}))
            for receipt in [
                {'status':'ASSETS_PREPARED','selected':0,'passed':0,'physicalAcceptance':False},
                {'status':'PASS','selected':24,'passed':24,'matrix':'claude/catalog-fidelity','stageCompleted':True,'matrixCompleted':False,'physicalAcceptance':False},
                {'status':'PASS','selected':24,'passed':24,'matrix':'all','stageCompleted':True,'matrixCompleted':True,'physicalAcceptance':True},
            ]:
                (root / 'matrix.log').write_text(json.dumps(receipt) + '\n')
                with self.assertRaises(RuntimeError):
                    assets.matrix_result(root, 'vz', 'a' * 40)

    def test_matrix_requires_actual_nonzero_output_and_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            completion = {'backend': 'vz', 'sourceCommit': 'a' * 40,
                          'matrixCompleted': True, 'physicalAcceptance': True, 'stageCompleted': False, 'matrix': 'all', 'cleanupFailed': []}
            (root / 'completion-1.json').write_text(json.dumps(completion))
            for passed, selected, ok in [(24, 24, True), (0, 0, False), (23, 24, False), (24, 0, False)]:
                (root / 'matrix.log').write_text(json.dumps({'status': 'PASS', 'selected': selected, 'passed': passed, 'matrix': 'all', 'physicalAcceptance': True, 'matrixCompleted': True}) + '\n')
                if ok:
                    self.assertEqual(assets.matrix_result(root, 'vz', 'a' * 40), 24)
                else:
                    with self.assertRaises(RuntimeError):
                        assets.matrix_result(root, 'vz', 'a' * 40)
            (root / 'matrix.log').write_text(json.dumps({'status':'PASS','selected':24,'passed':24,'matrix':'all','physicalAcceptance':True,'matrixCompleted':True}) + '\n')
            for field, value in [('matrixCompleted', False), ('matrix', 'claude/catalog-fidelity'), ('stageCompleted', True), ('physicalAcceptance', False), ('cleanupFailed', ['host']), ('sourceCommit', 'b' * 40)]:
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
                          'buildRegistryPort': 31000, 'tools': {key: str(tool) for key in assets.TOOLS},
                          'thinPool': {'name': 'suite', 'directory': str(root)}}
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

    @contextmanager
    def native_sdk_fixture(self, backend='vz'):
        # Controlled command responses only; no compiler, engine or VM runs.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            sdk = root / 'MacOSX.sdk'
            header = sdk / 'usr/include/TargetConditionals.h'
            header.parent.mkdir(parents=True)
            header.write_text('/* installed SDK header */\n')
            alias = root / 'selected.sdk'
            alias.symlink_to(sdk, target_is_directory=True)
            work = root / 'work'
            work.mkdir(mode=0o700)
            with socket.socket(socket.AF_UNIX) as sock:
                sock.bind(str(root / 'engine.sock'))
                tools = assets.TOOLS | ({'cargo', 'rustc', 'cc', 'go'} if backend == 'vz' else set())
                p = assets.Producer(backend, work, {'engineSocket': str(root / 'engine.sock'),
                                                 'tools': {key: '/tools/' + key for key in tools}})
                p.sdk_output = (str(alias) + '\n').encode()
                p.commands = []
                def call(argv, **kwargs):
                    p.commands.append((argv, dict(kwargs.get('env', p.env))))
                    if argv[0] == '/usr/bin/xcrun':
                        if isinstance(p.sdk_output, Exception):
                            raise p.sdk_output
                        return p.sdk_output
                    if argv[0] == '/usr/sbin/sysctl':
                        return b'1\n'
                    if argv[0] == '/tools/rustc':
                        return b'rustc 1.99.0 (fixture)\n'
                    if argv[0] == sys.executable:
                        return b'a' * 64
                    if argv[0] == '/tools/cc':
                        (p.files / 'nativeProbe').write_bytes(b'fixture probe')
                    return {'/tools/node': b'v24.0.0', '/tools/openssl': b'OpenSSL 3.0',
                            '/tools/bun': (ROOT / '.bun-version').read_bytes()}.get(argv[0], b'')
                p.call = call
                p.engine = lambda command, *args: json.dumps(
                    {'OSType': 'linux', 'Architecture': p.arch, 'ID': 'fixture-engine-id'} if command == 'info'
                    else {'Server': {'Version': '29.0', 'ApiVersion': '1.45'}}).encode()
                p.stage = lambda path, name, executable: {'path': str(path)}
                with patch.object(assets.platform, 'system', return_value='Darwin' if backend == 'vz' else 'Linux'), \
                     patch.object(assets.platform, 'machine', return_value='arm64' if backend == 'vz' else 'x86_64'), \
                     patch.object(p, 'snapshot', side_effect=RuntimeError('before_images')) as snapshot:
                    yield p, sdk, header, snapshot

    def test_native_sdk_selected_alias_reaches_cargo_and_probe_without_ambient_config(self):
        with patch.dict(os.environ, {'SDKROOT': '/poison/sdk', 'CC': '/poison/cc', 'CFLAGS': '-poison',
                                     'DEVELOPER_DIR': '/poison/developer', 'CARGO_BUILD_JOBS': '99'}), \
             self.native_sdk_fixture() as (p, sdk, header, snapshot):
            with self.assertRaisesRegex(RuntimeError, 'before_images'):
                p.produce()
            snapshot.assert_called_once()
            p.native_host()
            discovery = [entry for entry in p.commands if entry[0][0] == '/usr/bin/xcrun']
            self.assertEqual(len(discovery), 1)
            self.assertEqual(discovery[0][0], ['/usr/bin/xcrun', '--sdk', 'macosx', '--show-sdk-path'])
            self.assertFalse({'SDKROOT', 'CC', 'CFLAGS', 'DEVELOPER_DIR'}.intersection(discovery[0][1]))
            cargo = next(env for argv, env in p.commands if argv[0] == '/tools/cargo')
            probe = next(env for argv, env in p.commands if argv[0] == '/tools/cc')
            self.assertEqual(cargo.get('SDKROOT'), str(sdk))
            self.assertEqual(probe.get('SDKROOT'), str(sdk))
            self.assertEqual(cargo.get('CARGO_BUILD_JOBS'), '2')
            self.assertEqual(cargo['RUSTC'], '/tools/rustc')
            self.assertEqual(cargo['ENGRAM_EGRESS_PROXYD_FINGERPRINT'], 'a' * 64)
            self.assertFalse({'CC', 'CFLAGS', 'DEVELOPER_DIR'}.intersection(cargo))
            self.assertNotIn('SDKROOT', p.env)

    def test_native_sdk_rejects_bad_selection_before_images(self):
        for case in ['empty', 'relative', 'multiple', 'encoding', 'nul', 'missing', 'file', 'writable',
                     'foreign_owner', 'missing_header', 'empty_header', 'header_directory', 'unreadable_header',
                     'symlink_loop', 'xcrun_missing', 'xcrun_failure']:
            with self.subTest(case=case), self.native_sdk_fixture() as (p, sdk, header, snapshot):
                if case in ['empty', 'relative', 'multiple', 'encoding', 'nul', 'missing', 'file']:
                    p.sdk_output = {'empty': b'', 'relative': b'relative.sdk\n', 'multiple': b'/one\n/two\n',
                                    'encoding': b'\xff', 'nul': b'/sdk\x00', 'missing': str(sdk / 'missing').encode(),
                                    'file': str(header).encode()}[case]
                elif case == 'writable':
                    sdk.chmod(0o777)
                elif case == 'missing_header':
                    header.unlink()
                elif case == 'empty_header':
                    header.write_bytes(b'')
                elif case == 'header_directory':
                    header.unlink()
                    header.mkdir()
                elif case == 'symlink_loop':
                    loop = sdk.parent / 'loop.sdk'
                    loop.symlink_to(loop)
                    p.sdk_output = str(loop).encode()
                elif case in ['xcrun_missing', 'xcrun_failure']:
                    p.sdk_output = FileNotFoundError('xcrun') if case == 'xcrun_missing' else RuntimeError('producer_command')
                real_stat, real_access = Path.stat, os.access
                def sdk_stat(path, *args, **kwargs):
                    value = real_stat(path, *args, **kwargs)
                    if case == 'foreign_owner' and path == sdk:
                        fields = list(value)
                        fields[4] = os.getuid() + 10000
                        return os.stat_result(fields)
                    return value
                with patch.object(Path, 'stat', autospec=True, side_effect=sdk_stat), \
                     patch.object(assets.os, 'access', side_effect=lambda path, mode:
                                  False if case == 'unreadable_header' and path == header else real_access(path, mode)):
                    with self.assertRaisesRegex(RuntimeError, 'native_macos_sdk|producer_command'):
                        p.produce()
                snapshot.assert_not_called()
                self.assertFalse(any(argv[0] == '/tools/cargo' for argv, env in p.commands))

    def test_firecracker_preflight_does_not_discover_or_export_native_sdk(self):
        with self.native_sdk_fixture('firecracker') as (p, sdk, header, snapshot), \
             patch.object(assets.os, 'getuid', return_value=0), \
             patch.object(assets, 'canonical', return_value=header), \
             patch.object(assets.stat, 'S_ISCHR', return_value=True):
            with self.assertRaisesRegex(RuntimeError, 'before_images'):
                p.produce()
            snapshot.assert_called_once()
            self.assertFalse(any(argv[0] == '/usr/bin/xcrun' for argv, env in p.commands))
            self.assertNotIn('SDKROOT', p.env)
            self.assertNotIn('CARGO_BUILD_JOBS', p.env)

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
