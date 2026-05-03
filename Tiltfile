# Engram dev orchestration.
#
# `tilt up` brings up the full local stack:
#
#   ┌─ docker-compose (deploy/docker-compose.dev.yml) ─┐
#   │  postgres   :5435                                │
#   │  registry   :5000   (OCI Distribution Spec)      │
#   └──────────────────────────────────────────────────┘
#   ┌─ local processes ────────────────────────────────┐
#   │  bootstrap          one-shot — writes KEK to .env │
#   │  install-harnesses  one-shot — cross-compiles + pulls Claude │
#   │  vz-codesign        one-shot — Mac only           │
#   │  coordinator        cargo run, manual restart     │
#   │  web                pnpm dev, vite HMR in-process │
#   └──────────────────────────────────────────────────┘
#
# Visit http://localhost:10350 for Tilt's status dashboard;
# http://localhost:5173 for the SPA; http://127.0.0.1:8090 for the
# coordinator HTTP API directly.
#
# Why Tilt and not just `docker compose`: the coordinator runs via
# `cargo run` (incremental rebuilds, host-side debug symbols),
# while postgres + registry are containerised. Tilt is the
# integration point that wires foreground processes and compose
# services together with one supervisor + log aggregator + UI.
#
# Production deploys go through a Helm chart (not this file or
# `deploy/docker-compose.dev.yml`). Both dev paths intentionally
# stop short of "package the coordinator into a container" because
# that's the wrong shape for an inner-loop iteration tool.

# Tilt's safety guard refuses to run `local()` when the active
# kubectl context looks production-shaped (any GKE/EKS/AKS context
# matches by default). This Tiltfile doesn't deploy any Kubernetes
# resources — only docker-compose services and host processes —
# so allowing the current context here is safe regardless of what
# it points at. We don't pass the context to any k8s call.
allow_k8s_contexts(k8s_context())

# ----------------------------------------------------------------
# .env loader
#
# Tilt's Starlark can't mutate os.environ, so we parse .env into a
# dict and pass values through `serve_env` per-resource. Mirrors
# what `set dotenv-load := true` does for `just`.
# ----------------------------------------------------------------

def read_env_file(path):
    out = {}
    if not os.path.exists(path):
        return out
    for raw in str(read_file(path)).splitlines():
        s = raw.strip()
        if not s or s.startswith('#') or '=' not in s:
            continue
        k, v = s.split('=', 1)
        v = v.strip()
        if len(v) >= 2:
            if (v[0] == '"' and v[-1] == '"') or (v[0] == "'" and v[-1] == "'"):
                v = v[1:-1]
        out[k.strip()] = v
    return out

env_file = read_env_file('.env')

def env_or(key, default):
    """Look up a value: .env first, then process env, then default."""
    return env_file.get(key) or os.environ.get(key) or default

# ----------------------------------------------------------------
# Arch dispatch — VZ on Apple Silicon, Firecracker on Linux+KVM.
# Process backend isn't part of the dev loop anymore (test-only).
# ----------------------------------------------------------------

uname_str = str(local('uname -s -m', echo_off=True, quiet=True)).strip()

if 'Darwin' in uname_str and 'arm64' in uname_str:
    sandbox_backend = 'vz'
    kernel_key = 'ENGRAM_VZ_KERNEL_PATH'
    kernel_default = os.environ.get('HOME', '') + '/.cache/engram-vz-test/vmlinux-arm64'
    kernel_pull_hint = '`just vz-pull-kernel` to populate the cache'
    needs_codesign = True
elif 'Linux' in uname_str and 'x86_64' in uname_str:
    sandbox_backend = 'firecracker'
    kernel_key = 'ENGRAM_KERNEL_IMAGE_PATH'
    kernel_default = os.environ.get('HOME', '') + '/.cache/engram-fc-test/vmlinux'
    kernel_pull_hint = ('`bash crates/engram-sandbox-firecracker/scripts/' +
                        'fetch-fc-test-artifacts.sh` to populate the cache')
    needs_codesign = False
else:
    fail('engram dev: unsupported host %r — Darwin/arm64 (VZ) or ' +
         'Linux/x86_64 (Firecracker) required' % uname_str)

kernel_path = env_or(kernel_key, kernel_default)
if not os.path.exists(kernel_path):
    fail(
        'kernel artifact not found at {path}. Run {hint}, ' +
        'or set {key} in .env to a vmlinux path you already have.'
            .format(path=kernel_path, hint=kernel_pull_hint, key=kernel_key)
    )

# ----------------------------------------------------------------
# Infra: postgres + registry via docker-compose.
# The compose file's `coordinator` service is profile-gated to
# `docker-only`, so this call brings up just the two infra services.
# ----------------------------------------------------------------

docker_compose('deploy/docker-compose.dev.yml')
dc_resource('postgres',
    labels=['infra'],
    links=['postgres://engram:engram@localhost:5435/engram'])
dc_resource('registry',
    labels=['infra'],
    links=['http://localhost:5001/v2/_catalog'])

# ----------------------------------------------------------------
# Setup one-shots: KEK + harness packs + (Mac only) codesign.
# ----------------------------------------------------------------

local_resource('bootstrap',
    cmd='just bootstrap',
    labels=['setup'])

local_resource('install-harnesses',
    cmd='just install-harnesses',
    # Re-run when harness sources change. The Claude binary download
    # is gated inside the recipe by a version check, so this stays
    # cheap on the cached path.
    deps=[
        'crates/engram-harness-noop/src',
        'crates/engram-harness-claude/src',
    ],
    labels=['setup'])

if needs_codesign:
    local_resource('vz-codesign',
        cmd='just vz-codesign',
        # Re-codesign after a coordinator-binary change so the
        # entitlement sticks across rebuilds.
        deps=[
            'crates/engram-coordinator/src',
            'crates/engram-sandbox-vz/src',
        ],
        labels=['setup'])

# ----------------------------------------------------------------
# Coordinator (cargo run, manual restart).
#
# Defaults to `auto_init=True` so it starts on `tilt up`, but
# `trigger_mode=TRIGGER_MODE_MANUAL` keeps it running through Rust
# edits. Click "rebuild" in Tilt's UI when you want to pick up
# code changes — we don't auto-restart because losing warm-pool
# state on every save during dev is more expensive than the
# benefit of "edits are live".
# ----------------------------------------------------------------

coord_env = {
    'DATABASE_URL': 'postgres://engram:engram@localhost:5435/engram',
    'ENGRAM_BIND_ADDR': '127.0.0.1:8090',
    'ENGRAM_MODE': 'all',
    'ENGRAM_SANDBOX_BACKEND': sandbox_backend,
    'ENGRAM_SANDBOX_WORK_DIR': './var/sandboxes',
    'ENGRAM_LOCAL_PATH': './var/engram',
    kernel_key: kernel_path,
    'ENGRAM_DEFAULT_IMAGE': env_or('ENGRAM_DEFAULT_IMAGE', 'warm-1'),
    'ENGRAM_WARM_POOL_SIZE': env_or('ENGRAM_WARM_POOL_SIZE', '1'),
    'ENGRAM_KEK_MASTER_KEY': env_or('ENGRAM_KEK_MASTER_KEY', ''),
    'RUST_LOG': 'info,engram=debug',
}

coord_deps = ['postgres', 'registry', 'bootstrap', 'install-harnesses']
if needs_codesign:
    coord_deps.append('vz-codesign')

local_resource('coordinator',
    serve_cmd='cargo run -p engram-coordinator',
    serve_env=coord_env,
    resource_deps=coord_deps,
    readiness_probe=probe(
        period_secs=2,
        timeout_secs=2,
        http_get=http_get_action(port=8090, path='/healthz'),
    ),
    links=[
        link('http://127.0.0.1:8090/healthz', 'healthz'),
        link('http://127.0.0.1:8090/api/registries', 'registries'),
    ],
    labels=['app'],
    trigger_mode=TRIGGER_MODE_MANUAL,
    auto_init=True)

# ----------------------------------------------------------------
# Web SPA (vite dev server).
#
# Vite's HMR runs in-process — Tilt should NEVER restart this.
# Edits to web/src/** are picked up via vite's own file watcher,
# not via a Tilt re-run. We deliberately don't pass `deps` here.
# ----------------------------------------------------------------

local_resource('web',
    serve_cmd='cd web && pnpm install --silent && pnpm dev --strictPort',
    resource_deps=['coordinator'],
    readiness_probe=probe(
        period_secs=2,
        timeout_secs=2,
        http_get=http_get_action(port=5173, path='/'),
    ),
    links=[
        link('http://localhost:5173/', 'overview'),
        link('http://localhost:5173/settings', 'settings'),
    ],
    labels=['app'],
    trigger_mode=TRIGGER_MODE_MANUAL,
    auto_init=True)

# Resources are grouped in the Tilt UI by `labels` above:
# infra (postgres, registry) → setup (bootstrap, harnesses, codesign)
# → app (coordinator, web). Click any one to jump to its log stream
# / readiness state / restart button.
