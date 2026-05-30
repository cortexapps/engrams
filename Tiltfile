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
#   │  coordinator        cargo run (Linux) or          │
#   │                     build+codesign+exec (Mac)     │
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
# Backend + topology — one probe, no arch ladder (ADR 0024).
#
# `detect-backend.sh` is the single host-capability source of truth
# (the same script the bake/kernel recipes use): /dev/kvm -> firecracker,
# macOS+arm64 -> vz, else -> process. The binaries keep their explicit
# ENGRAM_SANDBOX_BACKEND; we just hand them the concrete value here.
#
# The probe drives two coupled decisions:
#   • backend  — passed verbatim as ENGRAM_SANDBOX_BACKEND.
#   • topology — `process` runs the coordinator in-process (mode=all,
#     no host-agent, which has no Process backend); every real-virt
#     backend runs the prod-shape split (coord mode=coordinator +
#     host-agent). Split is the default; `process` is the auto-degrade.
# ----------------------------------------------------------------

uname_str = str(local('uname -s -m', echo_off=True, quiet=True)).strip()
sandbox_backend = str(
    local('bash deploy/dev/detect-backend.sh', echo_off=True, quiet=True)
).strip()

# Split (prod-shape) for the real-virt backends; in-process for `process`.
dev_split = sandbox_backend != 'process'
# VZ is the only backend that needs the macOS virtualization entitlement,
# so codesigning is gated on it (not just "is macOS").
needs_codesign = sandbox_backend == 'vz'

# Kernel artifact: only the real-virt backends boot one. Pick the env
# var + cache default that matches the chosen backend; the binary reads
# whichever one corresponds to its backend.
kernel_key = None
kernel_path = None
if sandbox_backend == 'vz':
    kernel_key = 'ENGRAM_VZ_KERNEL_PATH'
    kernel_default = os.environ.get('HOME', '') + '/.cache/engram-vz-test/vmlinux-arm64'
    kernel_pull_hint = '`just pull-kernel` to populate the cache'
elif sandbox_backend == 'firecracker':
    kernel_key = 'ENGRAM_KERNEL_IMAGE_PATH'
    kernel_default = os.environ.get('HOME', '') + '/.cache/engram-fc-test/vmlinux'
    kernel_pull_hint = '`just pull-kernel` to populate the cache'

if kernel_key:
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

# On Linux, layer in the host-networking override for fake-gcs-server
# (see deploy/docker-compose.linux.yml). macOS uses the base file's
# published-port form — host networking is unreachable from the Mac
# host through Docker Desktop's Linux VM.
compose_files = ['deploy/docker-compose.dev.yml']
if 'Linux' in uname_str:
    compose_files.append('deploy/docker-compose.linux.yml')
docker_compose(compose_files)
dc_resource('postgres',
    labels=['infra'],
    links=['postgres://engram:engram@localhost:5435/engram'])
dc_resource('registry',
    labels=['infra'],
    links=['http://localhost:5001/v2/_catalog'])
# GCS emulator for cold-tier blob durability (ADR 0005 / Stage 4).
# The Rust SDK rewrites endpoints to `STORAGE_EMULATOR_HOST` whenever
# that env var is set; the coordinator + host-agent both honor it.
dc_resource('fake-gcs-server',
    labels=['infra'],
    links=['http://localhost:4443/storage/v1/b'])
# Jaeger — OTLP trace collector + UI for ADR 0019 cold-boot tracing.
# coord (+ host-agent in split mode) export here via
# OTEL_EXPORTER_OTLP_ENDPOINT, defaulted below.
dc_resource('jaeger',
    labels=['infra'],
    links=[link('http://localhost:16686', 'jaeger UI')])

# ----------------------------------------------------------------
# Setup one-shots: KEK + GCS bucket seed + (Mac only) codesign.
#
# Built-in harnesses are baked into the image (ADR 0021); `just
# bake-demo` builds the Claude harness from source, publishes it to the
# local registry, and bakes deploy/demo-claude/ against it.
# ----------------------------------------------------------------

local_resource('bootstrap',
    cmd='just bootstrap',
    labels=['setup'])

# Seed the cold-tier blob bucket in fake-gcs-server. Idempotent: the
# script POSTs the bucket and treats 200, 409, and "already exists"
# as success. Re-runs on each `tilt up`; cheap.
local_resource('seed-buckets',
    cmd='bash deploy/dev/seed-buckets.sh',
    resource_deps=['fake-gcs-server'],
    labels=['setup'])

# ----------------------------------------------------------------
# Coordinator (cargo run, manual restart).
#
# Defaults to `auto_init=True` so it starts on `tilt up`, but
# `trigger_mode=TRIGGER_MODE_MANUAL` keeps it running through Rust
# edits. Click "rebuild" in Tilt's UI when you want to pick up
# code changes — we don't auto-restart because losing the in-memory
# chunk cache / OCI client / scheduler state on every save during
# dev is more expensive than the benefit of "edits are live".
#
# On macOS the build → codesign → exec sequence is atomic: cargo
# rebuild produces fresh unsigned bytes, so codesign has to run
# AFTER the build but BEFORE the exec. Splitting into separate Tilt
# resources broke that ordering (codesign would run, then `cargo
# run` would rebuild and overwrite the signature). Inlining keeps
# the chain tight.
# ----------------------------------------------------------------

# ADR 0024: topology follows the backend probe (above) — `dev_split`
# is True for every real-virt backend (coord mode=coordinator + a
# separate host-agent, matching prod) and False only for `process`
# (coord mode=all, in-process backend, no host-agent to manage).
# This is no longer an env flag; the host's capabilities decide.

# CI consumes prebuilt release binaries via ENGRAM_INTEG_BIN_DIR
# (e.g. target/release) so `tilt ci` doesn't recompile. When set, the
# serve_cmds `exec` the binary directly instead of `cargo run` /
# build+codesign. Empty in normal dev.
bin_dir = env_or('ENGRAM_INTEG_BIN_DIR', '')

# ADR 0019: default the OTLP export target to the local Jaeger (dc above).
# Both coord and host-agent honor it; engram-telemetry is inert if it ever
# points nowhere, so a stale value never breaks startup. Override in .env
# to ship spans elsewhere (or to '' to disable export entirely).
otel_endpoint = env_or('OTEL_EXPORTER_OTLP_ENDPOINT', 'http://localhost:4317')

coord_env = {
    'DATABASE_URL': 'postgres://engram:engram@localhost:5435/engram',
    'ENGRAM_BIND_ADDR': '127.0.0.1:8090',
    'ENGRAM_MODE': 'coordinator' if dev_split else 'all',
    'ENGRAM_SANDBOX_BACKEND': sandbox_backend,
    'ENGRAM_SANDBOX_WORK_DIR': './var/sandboxes',
    'ENGRAM_LOCAL_PATH': './var/engram',
    'ENGRAM_DEFAULT_IMAGE': env_or('ENGRAM_DEFAULT_IMAGE', 'warm-1'),
    'ENGRAM_KEK_MASTER_KEY': env_or('ENGRAM_KEK_MASTER_KEY', ''),
    # ADR 0005 / Stage 4: cold-tier blob durability. Default to the
    # local fs backend; flip to `gcs` against the fake-gcs-server
    # emulator by setting ENGRAM_BLOB_BACKEND=gcs in .env (the seed
    # script provisions the `engram-snapshots-test` bucket). Split
    # mode forces gcs to match prod's topology — the whole point of
    # the rig is to catch backend-config-coupling bugs.
    'ENGRAM_BLOB_BACKEND': 'gcs' if dev_split else env_or('ENGRAM_BLOB_BACKEND', 'local'),
    'ENGRAM_GCS_BUCKET': env_or('ENGRAM_GCS_BUCKET', 'engram-snapshots-test'),
    'STORAGE_EMULATOR_HOST': env_or('STORAGE_EMULATOR_HOST', 'http://localhost:4443'),
    'OTEL_EXPORTER_OTLP_ENDPOINT': otel_endpoint,
    'RUST_LOG': 'info,engram=debug',
}

# Kernel env var only applies to a real-virt backend (mode=all + vz, or
# the host-agent under split). `process` boots no kernel.
if kernel_key and not dev_split:
    coord_env[kernel_key] = kernel_path

# The process backend is insecure (un-isolated host subprocesses), so
# the binary refuses to start it unless explicitly allowed. `just dev`
# on a virt-less box is exactly the sanctioned dev case, so opt in here.
if sandbox_backend == 'process':
    coord_env['ENGRAM_ALLOW_INSECURE_PROCESS_BACKEND'] = '1'

# `mke2fs` is keg-only under homebrew/e2fsprogs, so it isn't on the
# default PATH on Apple Silicon. The host-agent's harness substrate
# builder shells out to it, so the coordinator process needs it
# resolvable. Mirror what `just bake` does and prepend the keg path.
if 'Darwin' in uname_str:
    coord_env['PATH'] = (
        '/opt/homebrew/opt/e2fsprogs/sbin:' + os.environ.get('PATH', '')
    )

if bin_dir:
    # CI: run the downloaded release binary, no compile.
    coord_serve_cmd = 'exec ' + bin_dir + '/engram-coordinator'
elif needs_codesign:
    coord_serve_cmd = (
        'cargo build -p engram-coordinator && ' +
        'bash crates/engram-sandbox-vz/scripts/codesign.sh debug && ' +
        'exec ./target/debug/engram-coordinator'
    )
else:
    coord_serve_cmd = 'cargo run -p engram-coordinator'

local_resource('coordinator',
    serve_cmd=coord_serve_cmd,
    serve_env=coord_env,
    resource_deps=['postgres', 'registry', 'fake-gcs-server', 'jaeger', 'seed-buckets', 'bootstrap'],
    # Tilt's HTTP probe opens a fresh loopback TCP connection per
    # tick AND issues an HTTP request that makes the server log it.
    # On macOS the closed sockets sit in TIME_WAIT for 2*MSL=30s
    # against an ephemeral range of ~16k ports, and the kernel's
    # TIME_WAIT GC is lazy enough that a steady probe rate piles up
    # faster than it drains. Once the pool is full, `connect()`
    # starts returning EAGAIN ("resource temporarily unavailable")
    # and the probe enters a self-perpetuating failure mode that
    # also breaks every other connect to the same port. tcp_socket
    # is the cheapest probe Tilt offers — still one connect per
    # tick, but at period_secs=30 the steady-state TW count is
    # bounded at ~1 per service, well under the threshold.
    readiness_probe=probe(
        period_secs=30,
        timeout_secs=2,
        tcp_socket=tcp_socket_action(port=8090),
    ),
    links=[
        link('http://127.0.0.1:8090/healthz', 'healthz'),
        link('http://127.0.0.1:8090/api/registries', 'registries'),
    ],
    labels=['app'],
    trigger_mode=TRIGGER_MODE_MANUAL,
    auto_init=True)

# ----------------------------------------------------------------
# Host-agent (split-mode only).
#
# In `mode=all` the coordinator binary serves both the HTTP API
# and a local in-process backend. In `mode=coordinator` it serves
# only the API, and a separate `engram-host-agent` process owns
# the sandbox backend (FC/VZ), heartbeats over HTTP, and answers
# gRPC HostService calls. Production runs this split shape; dev runs
# it too whenever the backend probe picked a real-virt backend
# (ADR 0024) — only `process` collapses to coord-only mode=all.
# ----------------------------------------------------------------

# ADR 0018 M4: ENGRAM_INTEG_TWO_HOSTS=1 launches a SECOND host-agent so
# coord sees a 2-host cluster — the alive-source evac path has somewhere
# to relocate onto. From the operator's side it's still just `just dev`
# (or `tilt up`); the env flag adds host-agent-b.
two_hosts = env_or('ENGRAM_INTEG_TWO_HOSTS', '') in ('1', 'true', 'yes')

def _split_nbd():
    # Two host-agents on one box need disjoint NBD device sets (FC rootfs
    # page-in binds /dev/nbdN). VZ doesn't use NBD; single-host lets the
    # agent auto-discover (return None => leave ENGRAM_NBD_DEVICES unset,
    # preserving today's behavior).
    if not two_hosts or sandbox_backend != 'firecracker':
        return (None, None)
    listing = str(local("ls -1 /dev/nbd* 2>/dev/null || true",
                        echo_off=True, quiet=True)).strip()
    devs = [d for d in listing.split('\n') if d]
    if not devs:
        return (None, None)
    half = max(1, len(devs) // 2)
    return (','.join(devs[:half]), ','.join(devs[half:]))

nbd_a, nbd_b = _split_nbd()

def host_agent_resource(name, grpc_port, metrics_port, work_dir, nbd_csv):
    env = {
        # Same per-image kernel as the coord-side mode=all path uses.
        kernel_key: kernel_path,
        'ENGRAM_SANDBOX_WORK_DIR': work_dir,
        'ENGRAM_SANDBOX_BACKEND': sandbox_backend,
        # gRPC plumbing — coord dials advertise, host-agent listens on
        # bind. Same machine in dev, so loopback works for both.
        'ENGRAM_GRPC_LISTEN_ADDR': '127.0.0.1:' + grpc_port,
        'ENGRAM_GRPC_ADVERTISE_ADDR': 'http://127.0.0.1:' + grpc_port,
        'ENGRAM_COORDINATOR_ENDPOINT': 'http://127.0.0.1:8090',
        # Same GCS backend the coord uses, so chunks materialized
        # coord-side are reachable from the PooledBackend at runtime.
        'ENGRAM_BLOB_BACKEND': 'gcs',
        'ENGRAM_GCS_BUCKET': env_or('ENGRAM_GCS_BUCKET', 'engram-snapshots-test'),
        'STORAGE_EMULATOR_HOST': env_or('STORAGE_EMULATOR_HOST', 'http://localhost:4443'),
        # Egress proxy off in dev — set ENGRAM_EGRESS_PROXY_PORT to
        # enable. CI sets it (Blacksmith doesn't NAT FC TAP traffic, so
        # guests route via the proxy); local dev relies on host masquerade.
        'ENGRAM_EGRESS_PROXY_PORT': env_or('ENGRAM_EGRESS_PROXY_PORT', '0'),
        'ENGRAM_HOST_METRICS_ADDR': '0.0.0.0:' + metrics_port,
        # ADR 0019: same OTLP target as the coord, so the host-side
        # restore/boot spans land in the same Jaeger trace.
        'OTEL_EXPORTER_OTLP_ENDPOINT': otel_endpoint,
        'RUST_LOG': 'info,engram=debug',
    }
    if nbd_csv:
        env['ENGRAM_NBD_DEVICES'] = nbd_csv
    if 'Darwin' in uname_str:
        env['PATH'] = '/opt/homebrew/opt/e2fsprogs/sbin:' + os.environ.get('PATH', '')
    else:
        # Linux: the host-agent runs under sudo (below), which scrubs
        # PATH to a secure default. Preserve the caller's PATH so it
        # still finds firecracker / ip / iptables / mke2fs.
        env['PATH'] = os.environ.get('PATH', '')

    # How to produce + launch the binary:
    #   • bin_dir set (CI): exec the prebuilt release binary, no compile.
    #   • else: cargo build, then launch the debug binary.
    if bin_dir:
        ha_bin = bin_dir + '/engram-host-agent'
        build_prefix = ''
    else:
        ha_bin = './target/debug/engram-host-agent'
        build_prefix = 'cargo build -p engram-host-agent && '

    if needs_codesign:
        # VZ (macOS): the binary needs the virtualization entitlement;
        # codesign after build, before exec. No sudo on macOS.
        serve_cmd = (
            build_prefix +
            ('bash crates/engram-sandbox-vz/scripts/codesign.sh debug && '
             if not bin_dir else '') +
            'exec ' + ha_bin
        )
    else:
        # Firecracker (Linux): the host-agent creates per-VM TAPs
        # (CAP_NET_ADMIN), so it runs under passwordless sudo. sudo
        # scrubs the environment, so preserve exactly the keys Tilt
        # injected (--preserve-env). Needs a NOPASSWD sudoers entry on
        # the box (the dev-vm skill's bootstrap-remote installs one).
        preserve = ','.join(env.keys())
        serve_cmd = (
            build_prefix +
            'exec sudo -n --preserve-env=' + preserve + ' ' + ha_bin
        )

    local_resource(name,
        serve_cmd=serve_cmd,
        serve_env=env,
        resource_deps=['coordinator'],
        readiness_probe=probe(
            period_secs=30,
            timeout_secs=2,
            tcp_socket=tcp_socket_action(port=int(grpc_port)),
        ),
        links=[
            link('http://127.0.0.1:' + metrics_port + '/metrics', 'metrics'),
        ],
        labels=['app'],
        trigger_mode=TRIGGER_MODE_MANUAL,
        auto_init=True)

if dev_split:
    host_agent_resource('host-agent', '9101', '9100', './var/host-sandboxes', nbd_a)
    if two_hosts:
        host_agent_resource('host-agent-b', '9102', '9110', './var/host-sandboxes-b', nbd_b)

# ----------------------------------------------------------------
# Web SPA (vite dev server).
#
# Vite's HMR runs in-process — Tilt should NEVER restart this.
# Edits to web/src/** are picked up via vite's own file watcher,
# not via a Tilt re-run. We deliberately don't pass `deps` here.
#
# Skipped when ENGRAM_SKIP_WEB is set (CI's `tilt ci` has no browser
# to drive the SPA, and pnpm install + a vite readiness wait only slow
# the e2e gate down). Mirrors integration-up.sh's ENGRAM_SKIP_WEB.
# ----------------------------------------------------------------

skip_web = env_or('ENGRAM_SKIP_WEB', '') in ('1', 'true', 'yes')

if not skip_web:
    local_resource('web',
        serve_cmd='cd web && pnpm install --silent && pnpm dev --strictPort',
        resource_deps=['coordinator'],
        # See the coordinator probe above — same loopback port-pool
        # constraint applies to vite.
        readiness_probe=probe(
            period_secs=30,
            timeout_secs=2,
            tcp_socket=tcp_socket_action(port=5173),
        ),
        links=[
            link('http://localhost:5173/', 'overview'),
            link('http://localhost:5173/settings', 'settings'),
        ],
        labels=['app'],
        trigger_mode=TRIGGER_MODE_MANUAL,
        auto_init=True)

# Resources are grouped in the Tilt UI by `labels` above:
# infra (postgres, registry) → setup (bootstrap) → app (coordinator,
# web). Click any one to jump to its log stream / readiness state /
# restart button.
