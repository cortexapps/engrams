#!/usr/bin/env python3
"""Fixed physical MCP producer. No default engine, inherited credentials or skip path."""
import argparse
import hashlib
import io
import ipaddress
import json
import os
from pathlib import Path
import platform
import re
import secrets
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import time

REPO = Path(__file__).resolve().parents[2]
CI = 'tests/fixtures/mcp-stack/platform/ci'
LABEL = 'io.engrams.mcp.ci'
LEGACY_RUNTIME = 'crates/engram-coordinator/var'
DIGEST = re.compile(r'sha256:[a-f0-9]{64}')
SOURCE_IMAGES = {
    'coordinator': 'docker/coordinator.Dockerfile',
    'orchestrator': 'docker/orchestrator.Dockerfile',
    'worker': 'docker/mcp-worker.Dockerfile',
    'fixture': 'tests/fixtures/mcp-stack/platform/fixture.Dockerfile',
    'guard': 'tests/fixtures/mcp-stack/platform/guard.Dockerfile',
    'guest': 'deploy/demo/Dockerfile',
}
TOOLS = {'node', 'bun', 'docker', 'openssl', 'skopeo', 'gh', 'oras', 'npm'}


def require(ok, seam):
    if not ok:
        raise RuntimeError(seam)


def canonical(path, kind='file', owner=True):
    p = Path(path)
    require(p.is_absolute() and str(p) == path and p.resolve() == p, 'canonical_path')
    for part in [p, *p.parents]:
        require(not part.is_symlink(), 'symlink_path')
    s = p.lstat()
    require(not owner or s.st_uid == os.getuid(), 'path_owner')
    if kind == 'file':
        require(stat.S_ISREG(s.st_mode) and s.st_nlink == 1 and s.st_size > 0, 'regular_file')
    elif kind == 'socket':
        require(stat.S_ISSOCK(s.st_mode), 'engine_socket')
    elif kind == 'directory':
        require(stat.S_ISDIR(s.st_mode), 'owned_directory')
    return p


def read_operator(path, backend):
    p = canonical(path, owner=False)
    s = p.stat()
    require(s.st_uid in {0, os.getuid()} and s.st_mode & 0o022 == 0 and s.st_size <= 16384, 'operator_file')
    o = json.loads(p.read_bytes())
    require(set(o) == {'version', 'backend', 'engineSocket', 'nativeHostIp', 'subnet',
                       'registryPort', 'buildRegistryPort', 'tools'}, 'operator_keys')
    require(o['version'] == 1 and o['backend'] == backend, 'operator_backend')
    need = TOOLS | ({'cargo', 'rustc', 'cc', 'go'} if backend == 'vz' else set())
    require(isinstance(o['tools'], dict) and set(o['tools']) == need, 'operator_tools')
    for path in o['tools'].values():
        # Tools can be installed by root. Only staged runtime copies need run ownership.
        p = canonical(path, owner=False)
        require(os.access(p, os.X_OK) and p.stat().st_mode & 0o022 == 0, 'tool_mode')
    canonical(o['engineSocket'], 'socket')
    net = ipaddress.ip_network(o['subnet'], strict=True)
    ip = ipaddress.ip_address(o['nativeHostIp'])
    require(net.version == 4 and net.prefixlen == 24 and str(net).startswith('10.')
            and str(net).split('.')[1] != '0' and ip.version == 4 and ip.is_private
            and any(ip in ipaddress.ip_network(cidr) for cidr in
                    ['10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16'])
            and not ip.is_loopback and ip not in net, 'operator_network')
    for key in ['registryPort', 'buildRegistryPort']:
        require(type(o[key]) is int and 20000 <= o[key] <= 59997, 'operator_port')
    require(o['buildRegistryPort'] not in range(o['registryPort'], o['registryPort'] + 4), 'operator_port_overlap')
    return o


def source_paths(names):
    for name in names:
        parts = Path(name).parts
        require(not name.startswith('/') and '..' not in parts and
                not {'var', '.git', '.superpowers'}.intersection(parts) and
                not any(part == '.env' or (part.startswith('.env.') and
                        not part.endswith(('.example', '.sample', '.template'))) for part in parts),
                'secret_source_context')


def digest(data):
    return 'sha256:' + hashlib.sha256(data).hexdigest()


def image_facts(raw, config, architecture):
    m, c = json.loads(raw), json.loads(config)
    require(m.get('schemaVersion') == 2 and 'manifests' not in m and
            m.get('config', {}).get('digest') == digest(config), 'archive_linkage')
    require(c.get('os') == 'linux' and c.get('architecture') == architecture, 'archive_platform')
    return {'id': digest(config), 'manifestDigest': digest(raw)}


def matrix_result(root, backend, head):
    results = []
    for line in (root / 'matrix.log').read_text().splitlines():
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict) and value.get('status') == 'PASS' and 'selected' in value:
            results.append(value)
    require(len(results) == 1 and type(results[0].get('selected')) is int and
            results[0]['selected'] == 24 and type(results[0].get('passed')) is int and results[0]['passed'] == 24, 'matrix_case_count')
    completions = list(root.glob('completion-*.json'))
    require(len(completions) == 1, 'matrix_completion')
    done = json.loads(completions[0].read_bytes())
    require(done.get('backend') == backend and done.get('sourceCommit') == head and
            done.get('matrixCompleted') is True and done.get('cleanupFailed') == [], 'matrix_cleanup')
    return 24


class Producer:
    def __init__(self, backend, root, operator):
        self.backend, self.root, self.o = backend, root, operator
        self.arch = 'amd64' if backend == 'firecracker' else 'arm64'
        self.run = 'mcp-ci-' + secrets.token_hex(12)
        self.names = []
        self.counter = 0
        self.cancelled = False
        self.child = None
        self.groups = {}
        self.nested_started = False
        self.engine_identity = None
        self.socket = canonical(operator['engineSocket'], 'socket').stat()
        self.tools = operator['tools']
        self.env = {'PATH': ':'.join(dict.fromkeys([str(Path(p).parent) for p in self.tools.values()] +
                                                 ['/usr/bin', '/bin', '/usr/sbin', '/sbin'])),
                    'TMPDIR': str(root), 'LANG': 'C', 'GIT_CONFIG_NOSYSTEM': '1',
                    'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_SYSTEM': '/dev/null',
                    'GIT_TERMINAL_PROMPT': '0', 'GIT_OPTIONAL_LOCKS': '0',
                    'GIT_NO_LAZY_FETCH': '1'}
        self.config = root / 'producer-engine-config'
        self.config.mkdir(mode=0o700)
        self.adapter_config = root / 'engine-config'
        self.adapter_config.mkdir(mode=0o700)
        self.auth = root / 'anonymous-auth.json'
        self.put(self.auth, b'{"auths":{}}')
        self.put(root / 'registries.conf', b'unqualified-search-registries = []\n')
        self.put(root / 'policy.json', b'{"default":[{"type":"insecureAcceptAnything"}]}')
        (root / 'registries.d').mkdir(mode=0o700)
        (root / 'certs').mkdir(mode=0o700)
        self.source = root / 'source'
        self.files = root / 'files'
        self.files.mkdir(mode=0o700)
        self.images = {}
        self.head = ''
        self.adapter_started = False
        self.adapter_clean = False

    def put(self, path, data):
        with path.open('xb') as f:
            f.write(data)
        path.chmod(0o600)

    def group_event(self, child, state):
        with (self.root / 'process-groups.jsonl').open('a') as journal:
            journal.write(json.dumps({'pgid': child.pid, 'exit': child.returncode,
                                      'state': state}) + '\n')

    def group_exists(self, pgid):
        require(pgid in self.groups, 'unowned_process_group')
        try:
            os.killpg(pgid, 0)
            return True
        except ProcessLookupError:
            return False
        except PermissionError:
            # An inaccessible or exiting group is still unresolved. Only ESRCH
            # proves absence; keep polling within the retained group's deadline.
            return True

    def drain_group(self, child, grace):
        require(self.groups.get(child.pid) is child, 'unowned_process_group')
        def send(sig):
            try:
                os.killpg(child.pid, sig)
            except (ProcessLookupError, PermissionError):
                # Retain the group; bounded polling must still prove ESRCH.
                pass
        def wait_group(seconds):
            deadline = time.monotonic() + seconds
            while True:
                child.poll()  # Reap the leader even when closed pipes already drained.
                if not self.group_exists(child.pid):
                    return True
                if time.monotonic() >= deadline:
                    return False
                time.sleep(0.01)
        if self.group_exists(child.pid):
            send(signal.SIGTERM)
        if not wait_group(grace):
            send(signal.SIGKILL)
        if not wait_group(2):
            self.group_event(child, 'survivor')
            raise RuntimeError('producer_process_survivor')
        child.wait(timeout=2)
        self.group_event(child, 'absent')
        del self.groups[child.pid]
        if self.child is child:
            self.child = None

    def call(self, argv, *, env=None, data=None, timeout=7200, cleanup=False, cwd=None, grace=15):
        require(cleanup or not self.cancelled, 'cancelled')
        self.counter += 1
        number = self.counter
        log = self.root / f'command-{number:04d}.log'
        with log.open('xb') as err:
            child = subprocess.Popen([str(x) for x in argv], cwd=cwd or REPO, env=env or self.env, start_new_session=True,
                                     stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=err)
            self.child = child
            self.groups[child.pid] = child
            self.group_event(child, 'created')
            out, failure = b'', None
            try:
                out, _ = child.communicate(data, timeout=timeout)
            except BaseException as error:
                failure = error
            try:
                # Leader/pipe completion never proves that this retained session
                # group has no descendant. Drain on success and every failure.
                self.drain_group(child, grace)
                remaining, _ = child.communicate(timeout=2)
                if failure is not None:
                    out = remaining
            finally:
                with (self.root / 'commands.jsonl').open('a') as journal:
                    journal.write(json.dumps({'command': number, 'exit': child.returncode,
                                              'outputBytes': len(out),
                                              'groupDrained': child.pid not in self.groups}) + '\n')
            if failure is not None:
                raise failure
            require(child.returncode == 0, 'producer_command')
            return out

    def private_checkout(self, source):
        source = Path(source)
        require(source.is_absolute() and source.resolve() == source and source.is_dir(), 'source_checkout')
        # Trust only this exact original directory in these bootstrap commands.
        # No safe.directory value reaches the private repository or the adapter.
        flags = ['/usr/bin/git', '-c', 'safe.directory=' + str(source),
                 '-c', 'core.fsmonitor=false', '-c', 'core.hooksPath=/dev/null']
        head = self.call([*flags, '-C', source, 'rev-parse', 'HEAD']).decode().strip()
        require(re.fullmatch('[a-f0-9]{40}', head), 'source_head')
        require(not self.call([*flags, '-C', source, 'status', '--porcelain', '--untracked-files=no']), 'source_dirty')
        template = self.root / 'git-empty-template'
        template.mkdir(mode=0o700)
        checkout = self.root / 'checkout'
        self.call(['/usr/bin/git', 'init', '--template=' + str(template), checkout])
        checkout.chmod(0o700)
        self.call([*flags, '-c', 'protocol.file.allow=always', '-C', checkout,
                   'fetch', '--depth=1', '--no-tags', '--no-write-fetch-head', source, head])
        self.call(['/usr/bin/git', '-C', checkout, '-c', 'core.hooksPath=/dev/null',
                   'checkout', '--detach', head])
        # Every subsequent Git read uses only this principal-owned exact HEAD.
        require(self.call(['/usr/bin/git', '-C', checkout, 'rev-parse', 'HEAD']).decode().strip() == head,
                'private_checkout_head')
        require(not self.call(['/usr/bin/git', '-C', checkout, 'status', '--porcelain']), 'private_checkout_dirty')
        require(self.call([*flags, '-C', source, 'rev-parse', 'HEAD']).decode().strip() == head, 'source_head_changed')
        for directory, dirs, files in os.walk(checkout, followlinks=False):
            for path in [Path(directory), *(Path(directory) / name for name in [*dirs, *files])]:
                state = path.lstat()
                require(state.st_uid == os.getuid() and not stat.S_ISLNK(state.st_mode), 'private_checkout_owner')
        config = self.call(['/usr/bin/git', '-C', checkout, 'config', '--local', '--name-only', '--list']).decode().splitlines()
        require(all(key in {'core.repositoryformatversion', 'core.filemode', 'core.bare', 'core.logallrefupdates',
                            'core.ignorecase', 'core.precomposeunicode'}
                    for key in config), 'private_checkout_config')
        return checkout, head

    def run_private_checkout(self):
        checkout, head = self.private_checkout(REPO)
        script = '.github/scripts/mcp-stack-assets.py'
        require((checkout / script).read_bytes() == Path(__file__).read_bytes(), 'producer_source_changed')
        self.nested_started = True
        env = dict(self.env)
        if os.environ.get('GITHUB_TOKEN'):
            env['GITHUB_TOKEN'] = os.environ['GITHUB_TOKEN']
        self.call([sys.executable, '-I', checkout / script, '--backend', self.backend,
                   '--operator', self.operator_path, '--work-root', self.root / 'execution',
                   '--private-checkout'], env=env, cwd=checkout, grace=240)
        return matrix_result(self.root / 'execution/run', self.backend, head)

    def engine(self, *args, cleanup=False):
        s = canonical(self.o['engineSocket'], 'socket').stat()
        require((s.st_dev, s.st_ino) == (self.socket.st_dev, self.socket.st_ino), 'engine_socket_changed')
        if args[0] != 'info' and self.engine_identity is not None:
            before = json.loads(self.engine('info', '--format', '{{json .}}', cleanup=cleanup))
            require(before['ID'] == self.engine_identity, 'engine_identity_changed')
        out = self.call([self.tools['docker'], '--config', self.config, '--host',
                         'unix://' + self.o['engineSocket'], *args], cleanup=cleanup)
        after = canonical(self.o['engineSocket'], 'socket').stat()
        require((after.st_dev, after.st_ino) == (s.st_dev, s.st_ino), 'engine_socket_changed')
        # Every mutation is bracketed by identity queries; do not recursively call engine.
        if self.engine_identity is not None and args[0] != 'info':
            info = json.loads(self.engine('info', '--format', '{{json .}}', cleanup=cleanup))
            require(info['ID'] == self.engine_identity, 'engine_identity_changed')
        return out

    def skopeo(self, *args):
        command, *rest = args
        require(command in {'copy', 'inspect'}, 'skopeo_command')
        auth = ['--authfile', str(self.auth)]
        if command == 'copy':
            auth += ['--src-no-creds', '--dest-no-creds', '--src-cert-dir', str(self.root / 'certs'),
                     '--dest-cert-dir', str(self.root / 'certs')]
        else:
            auth += ['--no-creds', '--cert-dir', str(self.root / 'certs')]
        return self.call([self.tools['skopeo'], '--policy', self.root / 'policy.json',
                          '--registries.d', self.root / 'registries.d', '--registries-conf', self.root / 'registries.conf',
                          '--override-os', 'linux', '--override-arch', self.arch,
                          command, *auth, *rest], env={**self.env, 'REGISTRY_AUTH_FILE': str(self.auth),
                                      'XDG_CONFIG_HOME': str(self.config)})

    def own_container(self, role, image, args=(), command=()):
        name = self.run + '-' + role
        self.names.append(name)  # Recover exact planned name after a lost create response.
        self.put(self.root / f'planned-{role}.json', json.dumps({'name': name, 'label': self.run}).encode())
        identity = self.engine('create', '--name', name, '--label', f'{LABEL}={self.run}',
                               '--pull', 'never', '--restart', 'no', '--pids-limit', '256',
                               '--memory', '512m', '--cpus', '2', '--security-opt', 'no-new-privileges',
                               *args, image, *command).decode().strip()
        require(re.fullmatch('[a-f0-9]{64}', identity), 'container_identity')
        return identity

    def archive(self, role, dockerfile, context=None, build_args=None, local=False):
        output = self.files / role if local else self.files / f'{role}.docker.tar'
        args = ['buildx', 'build', '--builder', 'default', '--platform', 'linux/' + self.arch,
                '--provenance=false', '--sbom=false', '--label', 'org.opencontainers.image.revision=' + self.head,
                '--label', f'{LABEL}={self.run}', '--build-arg', 'SOURCE_COMMIT=' + self.head,
                '--file', str((context or self.source) / dockerfile), '--output',
                ('type=local,dest=' if local else 'type=docker,dest=') + str(output)]
        for key, value in (build_args or {}).items():
            args += ['--build-arg', key + '=' + value]
        args += [str(context or self.source)]
        self.engine(*args)
        if local:
            return output
        archive = self.files / f'{role}.oci.tar'
        self.skopeo('copy', 'docker-archive:' + str(output), 'oci-archive:' + str(archive))
        return archive

    def import_image(self, role, archive):
        raw = self.skopeo('inspect', '--raw', 'oci-archive:' + str(archive))
        config = self.skopeo('inspect', '--raw', '--config', 'oci-archive:' + str(archive))
        facts = image_facts(raw, config, self.arch)
        load = self.files / f'{role}.load.tar'
        self.skopeo('copy', 'oci-archive:' + str(archive), 'docker-archive:' + str(load))
        self.engine('load', '--input', str(load))
        actual = json.loads(self.engine('image', 'inspect', facts['id']))[0]
        require(actual['Id'] == facts['id'] and actual['Os'] == 'linux' and actual['Architecture'] == self.arch,
                'loaded_image_platform')
        if role in {*SOURCE_IMAGES, 'hostAgent', 'hostBase'}:
            require(actual['Config'].get('Labels', {}).get('org.opencontainers.image.revision') == self.head,
                    'loaded_image_revision')
        self.images[role] = {**facts, 'archive': {'path': str(archive)}}
        return facts

    def stage(self, source, name, executable=False):
        p = self.files / name
        # Copy bytes to a fresh inode; executable pin must be the actual staged file.
        with Path(source).open('rb') as src, p.open('xb') as dst:
            shutil.copyfileobj(src, dst)
        p.chmod(0o700 if executable else 0o600)
        canonical(str(p))
        return {'path': str(p)}

    def snapshot(self):
        require(not self.call(['/usr/bin/git', 'status', '--porcelain', '--untracked-files=no']), 'source_dirty')
        self.head = self.call(['/usr/bin/git', 'rev-parse', 'HEAD']).decode().strip()
        require(re.fullmatch('[a-f0-9]{40}', self.head), 'source_head')
        entries = {}
        for entry in self.call(['/usr/bin/git', 'ls-tree', '-r', '-z', self.head]).split(b'\0')[:-1]:
            metadata, name = entry.split(b'\t', 1)
            name = os.fsdecode(name)
            # This exact upstream directory contains legacy runtime files.
            if name == LEGACY_RUNTIME or name.startswith(LEGACY_RUNTIME + '/'):
                continue
            mode = metadata.split()[0].decode()
            require(mode in {'100644', '100755', '160000'}, 'source_entry_type')
            entries[name] = mode
        source_paths(entries)
        require(entries, 'source_empty')
        # Literal names keep selection and archive bytes identical, including
        # names with newlines or Git pathspec metacharacters.
        archive = self.call(['/usr/bin/git', '--literal-pathspecs', 'archive', '--format=tar',
                             self.head, '--', *entries])
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            expected = {name: mode == '160000' for name, mode in entries.items()}
            for name in entries:
                for parent in Path(name).parents:
                    if parent != Path('.'):
                        expected[str(parent)] = True
            seen = set()
            for member in tar.getmembers():
                source_paths([member.name])
                require(member.name in expected and member.name not in seen and
                        (member.isdir() if expected[member.name] else member.isfile()), 'source_archive_member')
                seen.add(member.name)
            require(seen == set(expected), 'source_archive_selection')
            self.source.mkdir(mode=0o700)
            tar.extractall(self.source, filter='data')
        # Outputs are private and outside this tracked-only build context.
        for role in [*SOURCE_IMAGES.values(), '.github/scripts/mcp-stack-inputs.mts',
                     '.github/scripts/mcp-stack-fc.sh', '.github/scripts/mcp-stack-vz.sh',
                     'tests/fixtures/mcp-stack/platform/main.ts',
                     'tests/fixtures/mcp-stack/platform/host-agent.Dockerfile']:
            require((self.source / role).is_file(), 'source_recipe')

    def acquire_boot(self):
        downloads = self.root / 'boot-download'
        downloads.mkdir(mode=0o700)
        ghconfig = self.root / 'gh-config'
        ghconfig.mkdir(mode=0o700)
        # The only credential read. It reaches fixed GitHub/GHCR artifact children only.
        token = os.environ.get('GITHUB_TOKEN', '')
        gh_env = {**self.env, 'GH_CONFIG_DIR': str(ghconfig)}
        if token:
            gh_env['GH_TOKEN'] = token
        asset = ('vmlinux-engram-' if self.backend == 'firecracker' else 'Image-engram-') + '6.1.102-1'
        for suffix in ['', '.sha256']:
            self.call([self.tools['gh'], 'release', 'download', 'fc-kernel-6.1.102-1',
                       '--repo', 'cortexapps/engrams', '--pattern', asset + suffix,
                       '--output', downloads / (asset + suffix)], env=gh_env)
        want = (downloads / (asset + '.sha256')).read_text().split()[0]
        require(re.fullmatch('[a-f0-9]{64}', want) and
                hashlib.sha256((downloads / asset).read_bytes()).hexdigest() == want, 'kernel_checksum')
        kernel = self.stage(downloads / asset, 'kernel')
        if self.backend != 'firecracker':
            shutil.rmtree(ghconfig)
            return kernel, None
        gitlink = self.call(['/usr/bin/git', 'ls-tree', 'HEAD', 'third_party/firecracker']).decode().split()
        require(len(gitlink) >= 4 and gitlink[0] == '160000' and re.fullmatch('[a-f0-9]{40}', gitlink[2]), 'fork_gitlink')
        auth = self.root / 'fork-registry.json'
        self.put(auth, b'{"auths":{}}')
        try:
            if token:
                self.call([self.tools['oras'], 'login', '--registry-config', auth, '--username', 'x-access-token',
                           '--password-stdin', 'ghcr.io'], data=token.encode())
            reference = 'ghcr.io/cortexapps/engrams/firecracker:' + gitlink[2]
            immutable = self.call([self.tools['oras'], 'resolve', '--registry-config', auth, reference]).decode().strip()
            require(DIGEST.fullmatch(immutable), 'fork_manifest')
            self.call([self.tools['oras'], 'pull', '--registry-config', auth,
                       reference.split(':')[0] + '@' + immutable, '--output', downloads / 'fork'])
            self.put(self.root / 'fork-provenance.json', json.dumps({'gitlink': gitlink[2], 'manifest': immutable}).encode())
        finally:
            auth.unlink(missing_ok=True)
            # gh may create config state. Delete only this new task-owned config directory.
            shutil.rmtree(ghconfig)
        return kernel, downloads / 'fork/firecracker'

    def final_host(self, fork):
        base_archive = self.archive('hostBase', 'docker/host-agent.Dockerfile')
        base = self.import_image('hostBase', base_archive)
        image = self.images['registry']['id']
        port = self.o['buildRegistryPort']
        registry = self.own_container('build-registry', image,
            ['--network', 'bridge', '--publish', f'127.0.0.1:{port}:5000', '--cap-drop', 'ALL',
             '--read-only', '--tmpfs', '/var/lib/registry:rw,nosuid,nodev,size=8g',
             '--tmpfs', '/tmp:rw,nosuid,nodev,size=16m'])
        self.engine('start', registry)
        uri = f'127.0.0.1:{port}/{self.run}/host'
        # Probe registry with bounded skopeo copy retries; never select another server.
        for attempt in range(20):
            try:
                self.skopeo('copy', '--preserve-digests', '--dest-tls-verify=false',
                            'oci-archive:' + str(base_archive), 'docker://' + uri + ':base')
                break
            except RuntimeError:
                if attempt == 19 or self.cancelled:
                    raise
                time.sleep(0.5)
        raw = self.skopeo('inspect', '--raw', '--tls-verify=false', 'docker://' + uri + '@' + base['manifestDigest'])
        require(digest(raw) == base['manifestDigest'], 'host_base_registry')
        context = self.root / 'host-context'
        context.mkdir(mode=0o700)
        shutil.copyfile(self.source / 'tests/fixtures/mcp-stack/platform/host-agent.Dockerfile', context / 'Dockerfile')
        shutil.copyfile(fork, context / 'firecracker')
        checksum = hashlib.sha256((context / 'firecracker').read_bytes()).hexdigest()
        archive = self.archive('hostAgent', 'Dockerfile', context, {'HOST_IMAGE': uri + '@' + base['manifestDigest'],
                                                               'FIRECRACKER_SHA256': checksum})
        final = self.import_image('hostAgent', archive)
        container = self.own_container('extract-host', final['id'], ['--network', 'none', '--cap-drop', 'ALL'])
        result = {}
        for key, name in [('hostAgent', 'engram-host-agent'), ('helper', 'engram-uffd-handler'),
                          ('proxy', 'engram-egress-proxyd'), ('firecracker', 'firecracker')]:
            target = self.files / key
            self.engine('cp', container + ':/usr/local/bin/' + name, str(target))
            target.chmod(0o700)
            canonical(str(target))
            result[key] = {'path': str(target)}
        require(hashlib.sha256((self.files / 'firecracker').read_bytes()).hexdigest() == checksum, 'baked_fork_bytes')
        self.images.pop('hostBase')
        return result

    def native_host(self):
        build = self.root / 'native-build'
        build.mkdir(mode=0o700)
        for name in ['cargo', 'target', 'netstack']:
            (build / name).mkdir(mode=0o700)
        # Real cargo/rustc toolchain binaries, never credential-reading rustup shims.
        env = {**self.env, 'CARGO_HOME': str(build / 'cargo'), 'CARGO_TARGET_DIR': str(build / 'target'),
               'RUSTC': self.tools['rustc'], 'SQLX_OFFLINE': 'true'}
        version = self.call([self.tools['rustc'], '--version']).decode().split()
        require(version[1] == '1.99.0', 'native_rust_toolchain')
        fp = self.call([sys.executable, '-I', self.source / 'docker/egress-proxyd-fingerprint.py'], env=env, cwd=self.source).decode().strip()
        require(re.fullmatch('[a-f0-9]{64}', fp), 'proxy_fingerprint')
        self.call([self.tools['cargo'], 'build', '--locked', '--release', '--manifest-path', self.source / 'Cargo.toml',
                   '-p', 'engram-host-agent', '-p', 'engram-egress-proxyd'],
                  env={**env, 'ENGRAM_EGRESS_PROXYD_FINGERPRINT': fp}, cwd=self.source)
        host = self.stage(build / 'target/release/engram-host-agent', 'hostAgent', True)
        proxy = self.stage(build / 'target/release/engram-egress-proxyd', 'proxy', True)
        self.call(['/usr/bin/codesign', '--force', '--sign', '-', '--entitlements',
                   self.source / 'crates/engram-sandbox-vz/entitlements.plist', host['path']])
        self.call(['/usr/bin/codesign', '--verify', '--strict', host['path']])
        self.call([self.tools['cc'], '-std=c11', '-Wall', '-Wextra', '-Werror', '-O2',
                   self.source / 'tests/fixtures/mcp-stack/platform/native-process-identity.c',
                   '-o', self.files / 'nativeProbe'])
        (self.files / 'nativeProbe').chmod(0o700)
        # build.sh creates its own exact private caches and verifies Go1.26.2.
        (build / 'netstack').rmdir()
        self.call(['/bin/bash', self.source / 'sandbox-netstack/build.sh', 'build', build / 'netstack'], cwd=self.source)
        helper = self.stage(build / 'netstack/engram-sandbox-netstack', 'helper', True)
        return {'hostAgent': host, 'proxy': proxy, 'helper': helper, 'nativeProbe': {'path': str(self.files / 'nativeProbe')}}

    def produce(self):
        require(platform.system() == ('Linux' if self.backend == 'firecracker' else 'Darwin') and
                platform.machine() in (['x86_64'] if self.backend == 'firecracker' else ['arm64']), 'physical_platform')
        require(self.backend != 'firecracker' or os.getuid() == 0, 'fc_root')
        info = json.loads(self.engine('info', '--format', '{{json .}}'))
        require(info.get('OSType') == 'linux' and info.get('Architecture') in
                [self.arch, 'x86_64' if self.arch == 'amd64' else 'aarch64'] and
                re.fullmatch('[A-Za-z0-9:_-]{12,128}', info.get('ID', '')), 'engine_platform')
        self.engine_identity = info['ID']
        version = json.loads(self.engine('version', '--format', '{{json .}}'))
        server = version.get('Server', {})
        require(int(str(server.get('Version', '0')).split('.')[0]) >= 26 and
                tuple(map(int, str(server.get('ApiVersion', '0.0')).split('.'))) >= (1, 45), 'engine_version')
        if self.backend == 'firecracker':
            for device in ['/dev/kvm', '/dev/net/tun']:
                p = canonical(device, kind='device', owner=False)
                require(stat.S_ISCHR(p.stat().st_mode) and os.access(p, os.R_OK | os.W_OK), 'fc_device')
        else:
            require(self.call(['/usr/sbin/sysctl', '-n', 'kern.hv_support']).strip() == b'1', 'vz_hypervisor')
        require(self.call([self.tools['node'], '--version']).decode().startswith('v24.'), 'node24')
        require(self.call([self.tools['openssl'], 'version']).decode().startswith('OpenSSL 3.'), 'openssl3')
        expected_bun = (REPO / '.bun-version').read_text().strip()
        require(self.call([self.tools['bun'], '--version']).decode().strip() == expected_bun, 'bun_version')
        self.put(self.root / 'engine-provenance.json', json.dumps({
            'id': info['ID'], 'os': info['OSType'], 'architecture': info['Architecture'],
            'socketDevice': self.socket.st_dev, 'socketInode': self.socket.st_ino}).encode())
        self.snapshot()
        bins = {name: self.stage(self.tools[name], name, True) for name in ['node', 'bun', 'docker', 'openssl']}
        upstream = json.loads((self.source / CI / 'upstream-images.json').read_bytes())
        for role, value in upstream.items():
            archive = self.files / f'{role}.oci.tar'
            reference = 'docker://' + value['repository'] + '@' + value[self.arch]
            raw = self.skopeo('inspect', '--raw', reference)
            require(digest(raw) == value[self.arch], 'upstream_manifest_changed')
            # OCI and Docker-save use different manifest media types. Conversion
            # may change manifest bytes; final Inputs use actual OCI facts only.
            self.skopeo('copy', reference, 'oci-archive:' + str(archive))
            self.put(self.root / f'upstream-{role}.json', json.dumps(
                {'reference': reference, 'sourceManifest': digest(raw)}).encode())
            self.import_image(role, archive)
        for role, recipe in SOURCE_IMAGES.items():
            self.import_image(role, self.archive(role, recipe))
        linux = self.archive('linux-assets', CI + '/linux-assets.Dockerfile', local=True)
        for key in ['claudeHarness', 'codexHarness', 'modelCli', 'agentdBundle', 'sentinelBundle', 'guestToolsBundle']:
            bins[key] = self.stage(linux / key, key, key.endswith('Harness') or key == 'modelCli')
        match = re.search(r'pub const DEFAULT_INIT_SHIM: &str = r#"([\s\S]*?)"#;',
                          (self.source / 'crates/engram-rootfs-materializer/src/inject.rs').read_text())
        require(match is not None, 'production_init')
        self.put(self.files / 'guestInit', match.group(1).encode())
        (self.files / 'guestInit').chmod(0o700)
        bins['guestInit'] = {'path': str(self.files / 'guestInit')}
        bins['kernel'], fork = self.acquire_boot()
        host = self.final_host(fork) if self.backend == 'firecracker' else self.native_host()
        for key in ['hostAgent', 'helper']:
            bins[key] = host.pop(key)
        template = {'version': 1, 'backend': self.backend, 'architecture': self.arch, 'sourceCommit': self.head,
                    'engineSocket': self.o['engineSocket'], 'engineConfig': str(self.adapter_config),
                    'engineId': self.engine_identity, 'registryPort': self.o['registryPort'],
                    'subnet': self.o['subnet'], 'nativeHostIp': self.o['nativeHostIp'],
                    'binaries': bins, 'images': self.images, **host}
        self.put(self.root / 'template.json', json.dumps(template).encode())
        self.call([bins['node']['path'], '--experimental-transform-types', REPO / '.github/scripts/mcp-stack-inputs.mts',
                   '--backend', self.backend, '--template', self.root / 'template.json', '--output', self.root / 'inputs.json'])
        deps = self.root / 'dependency-cache'
        deps.mkdir(mode=0o700)
        self.put(deps / '.npmrc', b'')
        self.put(deps / '.bunfig.toml', b'')
        depenv = {**self.env, 'PATH': str(self.files) + ':' + self.env['PATH'],
                  'NPM_CONFIG_USERCONFIG': str(deps / '.npmrc'), 'NPM_CONFIG_GLOBALCONFIG': '/dev/null',
                  'NPM_CONFIG_CACHE': str(deps / 'npm'), 'BUN_INSTALL_CACHE_DIR': str(deps / 'bun'),
                  'XDG_CONFIG_HOME': str(deps), 'BUN_INSTALL': str(deps / 'bun-install')}
        self.call([self.tools['npm'], 'ci', '--ignore-scripts'], env=depenv, cwd=REPO / 'mcp-worker')
        self.call([bins['bun']['path'], 'install', '--frozen-lockfile', '--ignore-scripts'], env=depenv, cwd=REPO / 'orchestrator')
        require(not self.call(['/usr/bin/git', 'status', '--porcelain', '--untracked-files=no']), 'source_dirty')
        wrapper = '.github/scripts/mcp-stack-' + ('fc' if self.backend == 'firecracker' else 'vz') + '.sh'
        # The adapter output root MUST NOT exist; its parent is this canonical0700 root.
        require(not (self.root / 'run').exists(), 'run_root_exists')
        self.adapter_started = True
        self.call(['/bin/bash', REPO / wrapper, '--run-root', self.root / 'run',
                   '--artifacts', self.root / 'inputs.json'], env={**self.env, 'PATH': str(self.files) + ':' + self.env['PATH']}, grace=240)
        return matrix_result(self.root / 'run', self.backend, self.head)

    def cleanup(self):
        failed = []
        for child in list(getattr(self, 'groups', {}).values()):
            try:
                self.drain_group(child, 0.1)
                child.communicate(timeout=2)
            except Exception:
                failed.append('process-group-' + str(child.pid))
        if getattr(self, 'groups', {}):
            failed.append('process-groups')
        if getattr(self, 'nested_started', False):
            try:
                actual = json.loads((self.root / 'execution/summary.json').read_bytes())
                require(actual.get('cleanup') is True, 'private_producer_cleanup')
            except Exception:
                failed.append('private-producer')
        if getattr(self, 'adapter_started', False):
            try:
                run = self.root / 'run'
                completions = list(run.glob('completion-*.json'))
                require(len(completions) == 1, 'adapter_completion_missing')
                done = json.loads(completions[0].read_bytes())
                require(done.get('backend') == self.backend and done.get('sourceCommit') == self.head and
                        done.get('cleanupFailed') == [], 'adapter_cleanup_failed')
                actual = json.loads((run / 'cleanup.json').read_bytes())
                require(actual.get('success') is True and actual.get('containersRemaining') == 0 and
                        actual.get('networkRemaining') is False and
                        re.fullmatch('mcp-[a-f0-9]{24}', actual.get('runId', '')), 'adapter_cleanup_receipt')
                label = 'label=engrams.mcp.run=' + actual['runId']
                require(not self.engine('ps', '-aq', '--filter', label, cleanup=True).strip(), 'adapter_survivor')
                require(not self.engine('network', 'ls', '-q', '--filter', label, cleanup=True).strip(), 'adapter_network_survivor')
                self.adapter_clean = True
            except Exception:
                failed.append('adapter')
        for name in reversed(self.names):
            try:
                # Query by exact name through a successful daemon list, never assume query failure means absent.
                ids = self.engine('ps', '-aq', '--no-trunc', '--filter', 'name=^/' + name + '$', cleanup=True).decode().split()
                require(len(ids) <= 1, 'cleanup_identity')
                for identity in ids:
                    item = json.loads(self.engine('inspect', identity, cleanup=True))[0]
                    require(item['Name'] == '/' + name and item['Config'].get('Labels', {}).get(LABEL) == self.run,
                            'cleanup_foreign_resource')
                    self.engine('rm', '--force', identity, cleanup=True)
                require(not self.engine('ps', '-aq', '--filter', 'name=^/' + name + '$', cleanup=True).strip(), 'cleanup_survivor')
            except Exception:
                failed.append(name)
        self.put(self.root / 'producer-cleanup.json', json.dumps({'version': 1, 'failed': failed,
                 'adapterStarted': getattr(self, 'adapter_started', False),
                 'adapterCleanup': getattr(self, 'adapter_clean', False)}).encode())
        require(not failed, 'producer_cleanup_failed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend', choices=['firecracker', 'vz'], required=True)
    parser.add_argument('--operator', required=True)
    parser.add_argument('--work-root', required=True)
    parser.add_argument('--private-checkout', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()
    os.umask(0o077)
    root = Path(args.work_root)
    require(root.is_absolute() and root.resolve() == root and root.parent.resolve() == root.parent and not root.exists() and
            not root.is_symlink(), 'work_root')
    root.mkdir(mode=0o700)
    producer, passed, cleanup = None, 0, False
    try:
        o = read_operator(args.operator, args.backend)
        producer = Producer(args.backend, root, o)
        producer.operator_path = args.operator
        if args.private_checkout:
            canonical(str(REPO), kind='directory')
            canonical(str(REPO / '.git'), kind='directory')
            require(REPO.name == 'checkout' and REPO.parent == root.parent and
                    (REPO.parent.stat().st_mode & 0o777) == 0o700, 'private_checkout_layout')
        def cancel(_signum, _frame):
            if not producer.cancelled:
                producer.cancelled = True
                raise RuntimeError('cancelled')
        signal.signal(signal.SIGTERM, cancel)
        signal.signal(signal.SIGINT, cancel)
        try:
            passed = producer.produce() if args.private_checkout else producer.run_private_checkout()
        finally:
            # Drain the interrupted child in call() before owned-resource cleanup.
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            producer.cleanup()
            cleanup = True
    finally:
        summary = {'version': 1, 'backend': args.backend, 'selected': 24, 'passed': passed,
                   'cleanup': cleanup, 'status': 'PASS' if passed == 24 and cleanup else 'FAIL'}
        path = root / 'summary.json'
        with path.open('x') as out:
            json.dump(summary, out)
        print(json.dumps(summary))
    require(passed == 24 and cleanup, 'physical_matrix_failed')


if __name__ == '__main__':
    try:
        main()
    except BaseException:
        # Private logs carry controlled evidence; CI output must not expose payloads or secrets.
        print('FAILED:mcp_physical_ci', file=sys.stderr)
        sys.exit(1)
