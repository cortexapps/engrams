#!/usr/bin/env python3
"""Exercise the rendered nginx configuration with synthetic callback values.

Requires Helm, Docker, and the image used by docker/web.Dockerfile already pulled.
No published ports or container network are needed. This does not test a cloud
load balancer, an authenticated RPC, or an external log collector.
"""

import argparse
import json
from pathlib import Path
import re
import signal
import subprocess
import time
import uuid


ROOT = Path(__file__).resolve().parents[4]
CHART = ROOT / 'deploy/helm/engram'
CALLBACK = '/integrations/mcp/callback'
RPC = '/rpc/engram.app.v1.IntegrationService/CompleteMcpAuthorization'
TOKENS = ['canary-code-8b', 'canary-state-8b', 'canary-issuer-8b',
          'canary-referrer-8b', 'canary-body-8b']
QUERY = '?code=' + TOKENS[0] + '&state=' + TOKENS[1] + '&iss=' + TOKENS[2]
SHELL = 'callback-canary-shell'


def run(*args, timeout=15, check=True):
    return subprocess.run(args, text=True, capture_output=True, check=check, timeout=timeout)


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def render(split):
    args = ['helm', 'template', 'callback-canary', str(CHART), '--namespace', 'canary',
            '-f', str(CHART / 'ci/render-values.yaml'), '--show-only',
            'templates/web-configmap.yaml', '--set', 'orchestrator.service.port=18081']
    if split:
        args += ['--set', 'web.apiBaseUrl=https://api.canary.example']
    rendered = run(*args, timeout=30).stdout
    block = rendered.split('  default.conf.template: |\n', 1)[1]
    require(all(not line or line.startswith('    ') for line in block.splitlines()),
            'Unexpected ConfigMap block; cannot extract the nginx configuration')
    return rendered, '\n'.join(line[4:] for line in block.splitlines()) + '\n'


def exercise(stage, image, split=False, unsafe=False):
    """Run one bounded container. The fixture upstream is not an RPC implementation."""
    stage.mkdir()
    config_dir = stage / 'config'
    assets = stage / 'assets'
    config_dir.mkdir()
    assets.mkdir()
    index = assets / 'index.html'
    index.write_text(SHELL)
    rendered, config = render(split)
    (stage / 'rendered.yaml').write_text(rendered)
    if unsafe:
        # Deliberate negative control: reinstate request-bearing access/error logs
        # at the server level, without changing request routing or test traffic.
        config = config.replace('server_name _;', 'server_name _;\n'
                                '  access_log /dev/stdout combined;\n'
                                '  error_log /dev/stderr info;', 1)
        require('access_log /dev/stdout combined;' in config, 'Negative control was not installed')
    (config_dir / 'default.conf.template').write_text(config)
    # Only the upstream is a fixture. The chart's server and log directives are
    # used without path, port, filesystem, or logging rewrites.
    (config_dir / 'upstream.conf.template').write_text('''server {
    listen 18081;
    access_log off;
    error_log /dev/null;
    location / { return 200 "callback-canary-upstream"; }
    location = /rpc/closed { return 444; }
}
''')
    name = 'mcp-callback-canary-' + uuid.uuid4().hex[:12]
    manifest = {'name': name, 'image': image, 'split': split, 'negative_control': unsafe,
                'container_id': None, 'removed': False}
    manifest_path = stage / 'manifest.json'
    manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
    container = None
    started = time.monotonic()
    count = 0

    def request(path, status, body='', malformed=False):
        nonlocal count
        method = 'POST' if body else 'GET'
        if body:
            result = run('docker', 'exec', container, 'wget', '-S', '-O-', '-T', '2',
                         '--header', f'Referer: https://referrer.canary.example/{TOKENS[3]}',
                         '--header', 'Content-Type: application/json', '--post-data', body,
                         'http://127.0.0.1:8080' + path, timeout=5, check=False)
            require(f'HTTP/1.1 {status} ' in result.stderr,
                    f'Unexpected POST status: {result.stderr}')
            count += 1
            return result.stdout
        data = (f'{method} {path} HTTP/1.1\r\nHost: canary.example\r\n'
                f'Referer: https://referrer.canary.example/{TOKENS[3]}\r\n'
                'Connection: close\r\n')
        if malformed:
            data += 'Host: duplicate.canary.example\r\n'
        data += '\r\n' + body
        result = run('docker', 'exec', container, 'sh', '-c',
                     'printf "%s" "$1" | nc -w 2 127.0.0.1 8080', 'canary', data, timeout=5)
        require(result.stdout.startswith(f'HTTP/1.1 {status} '),
                f'Unexpected response for {method} {path.split("?")[0]}: {result.stdout[:80]}')
        count += 1
        return result.stdout

    try:
        args = ['docker', 'run', '--detach', '--pull=never', '--name', name,
                '--label', 'engram.test=callback-ingress', '--network', 'none',
                '--cpus', '1', '--memory', '128m', '--pids-limit', '32',
                '--read-only', '--user', '101:101', '--cap-drop', 'ALL',
                '--security-opt', 'no-new-privileges',
                '--tmpfs', '/etc/nginx/conf.d:uid=101,gid=101,size=1m',
                '--tmpfs', '/var/cache/nginx:uid=101,gid=101,size=8m',
                '--tmpfs', '/var/run:uid=101,gid=101,size=1m',
                '--tmpfs', '/tmp:uid=101,gid=101,size=1m',
                '--add-host', 'callback-canary-orchestrator.canary.svc.cluster.local:127.0.0.1',
                '--mount', f'type=bind,source={config_dir},target=/etc/nginx/templates,readonly',
                '--mount', f'type=bind,source={assets},target=/usr/share/nginx/html,readonly',
                '--env', 'NGINX_ENVSUBST_FILTER=^ENGRAM_', image]
        container = run(*args, timeout=20).stdout.strip()
        manifest['container_id'] = container
        manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
        for _ in range(30):
            ready = run('docker', 'exec', container, 'wget', '-qO-',
                        'http://127.0.0.1:8080/healthz', timeout=3, check=False)
            if ready.returncode == 0 and ready.stdout == 'ok\n':
                break
            time.sleep(0.1)
        else:
            raise AssertionError('nginx did not become ready')
        effective = run('docker', 'exec', container, 'nginx', '-T')
        (stage / 'effective.conf').write_text(effective.stdout)
        (stage / 'config-check.log').write_text(effective.stderr)
        require(SHELL in request(CALLBACK + QUERY, 200), 'Callback did not serve the SPA')
        require(SHELL in request('/missing-asset' + QUERY, 200), 'SPA fallback did not run')
        request(RPC + QUERY, 200, json.dumps({'code': TOKENS[4]}))
        request('/rpc/closed' + QUERY, 502, json.dumps({'code': TOKENS[4]}))
        request(CALLBACK + QUERY, 400, malformed=True)
        index.unlink()
        request(CALLBACK + QUERY, 500)
        index.write_text(SHELL)
        if split:
            require('https://api.canary.example' in request('/engram-config.js', 200),
                    'Split-host runtime configuration was not served')
        require(count >= 6, 'No positive request count')
        run('docker', 'stop', '--time', '3', container, timeout=8)
        logs = run('docker', 'logs', container)
        (stage / 'stdout.log').write_text(logs.stdout)
        (stage / 'stderr.log').write_text(logs.stderr)
        if unsafe:
            require(all(token in logs.stdout for token in TOKENS[:4]),
                    'Access-log negative control did not expose the canary query and referrer')
            require(TOKENS[0] in logs.stderr and TOKENS[3] in logs.stderr,
                    'Error-log negative control did not expose the canary query and referrer')
        else:
            for token in TOKENS:
                require(token not in logs.stdout + logs.stderr, f'Canary value leaked: {token}')
            records = [json.loads(line) for line in logs.stdout.splitlines() if line.startswith('{')]
            require(len(records) == count, f'Expected {count} access records, got {len(records)}')
            expected = [('GET', CALLBACK, 200), ('GET', '/missing-asset', 200),
                        ('POST', RPC, 200), ('POST', '/rpc/closed', 502),
                        ('GET', CALLBACK, 400), ('GET', CALLBACK, 500)]
            if split:
                expected.append(('GET', '/engram-config.js', 200))
            require([(r['method'], r['path'], r['status']) for r in records] == expected,
                    'Request metadata did not survive fallback or error handling')
            require(all(r['duration'] >= 0 and r['bytes'] > 0 for r in records),
                    'Timing and size metadata is missing')
        print(f'{stage.name}: {count} requests, '
              f'{"negative control detected leaks" if unsafe else "no canary in nginx logs"}')
    finally:
        if container:
            # Capture startup failures too. Remove only the ID this invocation created.
            logs = run('docker', 'logs', container, check=False)
            (stage / 'stdout.log').write_text(logs.stdout)
            (stage / 'stderr.log').write_text(logs.stderr)
            run('docker', 'rm', '--force', container)
            require(run('docker', 'container', 'inspect', container, check=False).returncode != 0,
                    'Owned container still exists after cleanup')
            manifest['removed'] = True
        manifest['requests'] = count
        manifest['elapsed_seconds'] = round(time.monotonic() - started, 3)
        manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--stage', required=True, type=Path, help='New output directory under var/mcp-*')
    args = parser.parse_args()
    stage = args.stage.absolute()
    require(stage == stage.resolve(), 'Stage must not use symlinks')
    require(stage.is_relative_to(ROOT / 'var') and stage.relative_to(ROOT / 'var').parts[0].startswith('mcp-'),
            'Stage must be under this checkout var/mcp-*')
    stage.mkdir(parents=True, exist_ok=False)
    image, = re.findall(r'^FROM (nginx:\S+)$', (ROOT / 'docker/web.Dockerfile').read_text(), re.M)
    inspected = run('docker', 'image', 'inspect', image)
    (stage / 'image.json').write_text(inspected.stdout)
    metadata, = json.loads(inspected.stdout)
    require(metadata['Size'] <= 100 * 1024 * 1024, 'Canary image exceeds 100 MiB')
    # Use the exact locally inspected image ID in every run, even if the tag moves.
    def deadline(_signal, _frame):
        raise TimeoutError('Canary exceeded its 90 second bound')
    signal.signal(signal.SIGALRM, deadline)
    for name, split, unsafe in [('same-origin', False, False), ('split-host', True, False),
                                ('negative-control', False, True)]:
        signal.alarm(90)
        try:
            exercise(stage / name, metadata['Id'], split, unsafe)
        finally:
            signal.alarm(0)


if __name__ == '__main__':
    main()
