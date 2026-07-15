# Engram dev orchestration.
#
# `tilt up` brings up the full local stack:
#
#   ┌─ docker-compose (deploy/docker-compose.dev.yml) ─┐
#   │  postgres   :5435                                │
#   │  registry   :5000   (OCI Distribution Spec)      │
#   └──────────────────────────────────────────────────┘
#   ┌─ local processes ────────────────────────────────┐
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

# The KEK (and any other parse-time secret) must already be in .env
# before we read it: Tilt evaluates this at load time, so a value a
# runtime resource writes later arrives too late — the coordinator
# would boot with an empty ENGRAM_KEK_MASTER_KEY (the chicken-and-egg
# that forced a kill-Tilt-and-restart). `just bootstrap` generates the
# KEK on first run and no-ops if present, so run it HERE, at parse time,
# ahead of the read — not as a runtime resource.
local('just bootstrap', echo_off=True, quiet=True)

env_file = read_env_file('.env')

def env_or(key, default):
    """Look up a value: .env first, then process env, then default."""
    return env_file.get(key) or os.environ.get(key) or default

# ----------------------------------------------------------------
# ADR 0082: Firecracker dev via a dedicated Colima VM.
#
# When set, the host-agent (+ its FC stack) runs INSIDE the named Colima
# VM instead of as a Mac-local process — the only way to exercise the
# FC-only surfaces (NBD, UFFD, netns egress, squashfs patch-drives) on
# Apple Silicon, where /dev/kvm only exists inside a nested-virt guest.
# Coordinator/orchestrator/web/compose are untouched either way; only the
# backend probe and the host-agent resource below change shape. `just
# dev-fc` sets this; plain `just dev` / `tilt up` never does, so behavior
# with it unset is byte-for-byte what it was before this ADR.
# ----------------------------------------------------------------
fc_colima_profile = env_or('ENGRAM_FC_COLIMA_PROFILE', '')

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

# ADR 0082: the fc-colima bundles + host-agent build steps run under a real
# `nix develop` (for mksquashfs + the aarch64 musl cross-toolchain). The loop
# that a per-build `nix develop` used to feed is handled by the bundles
# resource's TRIGGER_MODE_MANUAL (see below), not by avoiding nix.
_nix = '$(command -v nix || echo /nix/var/nix/profiles/default/bin/nix)'

if fc_colima_profile:
    # The VM's /dev/kvm is invisible to a probe run on the Mac — force the
    # backend instead of asking detect-backend.sh, and fail fast (at parse
    # time, before any resource starts) if the VM hasn't been provisioned
    # or isn't running, rather than let the host-agent die deep into
    # `tilt up` with a confusing "no such file" for the kernel.
    _fc_colima_check = str(local(
        'colima ssh --profile ' + fc_colima_profile +
        ' -- sh -lc "test -f /opt/engram-dev/Image && ' +
        'test -x /opt/engram-dev/bin/mke2fs && echo ok || echo missing"',
        echo_off=True, quiet=True)).strip()
    if _fc_colima_check != 'ok':
        fail(
            ('ENGRAM_FC_COLIMA_PROFILE={p} is set but the Colima VM `{p}` has no ' +
             'guest kernel at /opt/engram-dev/Image, no mke2fs at ' +
             '/opt/engram-dev/bin/mke2fs, or the profile is not running. Run ' +
             '`just fc-colima-provision {p}` first.')
                .format(p=fc_colima_profile)
        )
    # ADR 0082: the docker-compose deps (postgres/registry/fake-gcs/jaeger) MUST
    # run on the Mac's docker, never inside the fc-dev VM — only the host-agent (a
    # `colima ssh` PROCESS, not a container) + its FC stack belong there. But
    # `colima start` persistently repoints the docker CLI at the VM daemon (writes
    # currentContext=colima-<profile> to ~/.docker/config.json), so a bare
    # `tilt up` would make docker_compose() deploy the deps INTO the VM — where
    # the VM's localhost→Mac DNAT (engram-dev-fwd) routes registry/GCS traffic
    # AWAY from them and the Mac-side coordinator can't see the DB. `just
    # dev-fc` pins DOCKER_HOST to the Mac docker; this
    # guard fails fast if that DIDN'T happen (e.g. a direct `tilt up` under the
    # stolen context) rather than silently misplacing the deps.
    _docker_host = os.environ.get('DOCKER_HOST', '')
    if not _docker_host:
        _docker_host = str(local(
            'docker context inspect --format "{{.Endpoints.docker.Host}}" 2>/dev/null || true',
            echo_off=True, quiet=True)).strip()
    if ('/' + fc_colima_profile + '/docker.sock') in _docker_host:
        fail(
            ("docker is pointed at the fc-dev VM daemon ({h}), so the compose " +
             "deps would deploy INTO the VM instead of the Mac (ADR 0082). Start " +
             "with `just dev-fc {p}` — it pins DOCKER_HOST to your Mac docker — or " +
             "run `docker context use colima` before `tilt up`.")
                .format(h=_docker_host, p=fc_colima_profile)
        )
    sandbox_backend = 'firecracker'
else:
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
    if fc_colima_profile:
        # Lives inside the VM, not on the Mac — the fail-fast check above
        # already confirmed it's there at exactly this path (the ADR 0082
        # provisioning contract), so there's nothing to stat locally.
        kernel_path = '/opt/engram-dev/Image'
    else:
        kernel_path = env_or(kernel_key, kernel_default)
        if not os.path.exists(kernel_path):
            fail(
                ('kernel artifact not found at {path}. Run {hint}, ' +
                 'or set {key} in .env to a vmlinux path you already have.')
                    .format(path=kernel_path, hint=kernel_pull_hint, key=kernel_key)
            )

# ----------------------------------------------------------------
# GCS emulator: reuse an external one if it's already up.
#
# `just dev` normally brings up its own fake-gcs-server (compose,
# profile `local-gcs`). But if another environment already has a
# fake-gcs-server bound at STORAGE_EMULATOR_HOST, starting a second one
# just collides on :4443 and the cold-tier setup cascades to failure.
# seed-buckets resource_deps on it; the coordinator merely points its
# GCS client at the endpoint and is deliberately not readiness-gated on
# either resource. So probe the endpoint at parse time: if something
# answers, treat it as external — skip our own container and point
# everything at the existing one. Set ENGRAM_USE_EXTERNAL_GCS=1 to
# force this without the probe (e.g. if the probe gives a false negative).
# ----------------------------------------------------------------
storage_emulator_host = env_or('STORAGE_EMULATOR_HOST', 'http://localhost:4443')
gcs_external = env_or('ENGRAM_USE_EXTERNAL_GCS', '') in ('1', 'true', 'yes')
if not gcs_external:
    gcs_probe = str(local(
        'curl -sf -o /dev/null --max-time 2 "' +
        storage_emulator_host + '/storage/v1/b" && echo up || echo down',
        echo_off=True, quiet=True)).strip()
    gcs_external = gcs_probe == 'up'
if gcs_external:
    print('engram dev: reusing external fake-gcs-server at %s (not starting our own)'
          % storage_emulator_host)

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
# fake-gcs-server lives behind the `local-gcs` profile (see the compose
# file). Activate it only when no external emulator was found.
compose_profiles = [] if gcs_external else ['local-gcs']
docker_compose(compose_files, profiles=compose_profiles)
dc_resource('postgres',
    labels=['infra'],
    links=['postgres://engram:engram@localhost:5435/engram'])
dc_resource('registry',
    labels=['infra'],
    links=['http://localhost:5001/v2/_catalog'])
# GCS emulator for cold-tier blob durability (ADR 0005 / Stage 4).
# The Rust SDK rewrites endpoints to `STORAGE_EMULATOR_HOST` whenever
# that env var is set; the coordinator + host-agent both honor it.
# Only registered when we're running our own (no external one found).
if not gcs_external:
    dc_resource('fake-gcs-server',
        labels=['infra'],
        links=[storage_emulator_host + '/storage/v1/b'])
# Jaeger — OTLP trace collector + UI for ADR 0019 cold-boot tracing.
# coord (+ host-agent in split mode) export here via
# OTEL_EXPORTER_OTLP_ENDPOINT, defaulted below.
dc_resource('jaeger',
    labels=['infra'],
    links=[link('http://localhost:16686', 'jaeger UI')])

# ----------------------------------------------------------------
# Setup one-shots: GCS bucket seed + (Mac only) codesign.
#
# (The KEK one-shot moved to a parse-time `just bootstrap` above — it
# has to land in .env before the parse-time read, so it can't be a
# runtime resource.)
#
# ADR 0062: the image bakes NO harness — the built-in `claude` rides the
# fleet `current_bundles` stamp (`just bundles-squashfs` / `bundles-vz`) and
# is selected per session. `just bake-demo` just bakes deploy/demo/.
# ----------------------------------------------------------------

# Seed the cold-tier blob bucket in fake-gcs-server. Idempotent: the
# script POSTs the bucket and treats 200, 409, and "already exists"
# as success. Re-runs on each `tilt up`; cheap. Runs against an external
# emulator too (the script honors STORAGE_EMULATOR_HOST) — harmless if
# the bucket already exists, and ensures it does if it doesn't. When the
# emulator is external it has no Tilt resource to gate on, so the dep is
# dropped; otherwise it waits on our own container.
#
# Nothing in the control plane resource_deps on this one-shot. GCS client
# construction is local and does not contact the emulator or inspect the
# bucket, so a broken cold-tier setup must not block the coordinator,
# orchestrator, or web development loops. Operations that actually need
# blob storage still fail at their point of use until seeding succeeds.
local_resource('seed-buckets',
    cmd=(
        'STORAGE_EMULATOR_HOST=' + storage_emulator_host + ' ' +
        'ENGRAM_GCS_BUCKET=' + env_or('ENGRAM_GCS_BUCKET', 'engram-snapshots-test') + ' ' +
        'bash deploy/dev/seed-buckets.sh'
    ),
    resource_deps=[] if gcs_external else ['fake-gcs-server'],
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
    # ADR 0051: the app-gRPC surface is how the orchestrator (and the
    # e2e_stack tests) drive the coordinator now that the web-facing REST
    # surface is gone. Defaults match config.rs (`app_grpc_addr`); the
    # bearer is fail-closed when `ENGRAM_APP_GRPC_TOKENS` is empty, so it
    # must be set for any gRPC client to connect. In split mode only the
    # coord needs these — the host-agent has no app-gRPC surface.
    'ENGRAM_APP_GRPC_ADDR': env_or('ENGRAM_APP_GRPC_ADDR', '127.0.0.1:50061'),
    'ENGRAM_APP_GRPC_TOKENS': env_or('ENGRAM_APP_GRPC_TOKENS', 'dev-app-grpc-token'),
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

if fc_colima_profile:
    # ADR 0082: the fc-dev VM has a ~19 GiB rootfs, smaller than the
    # production 20 GiB disk-cache floor. Lower BOTH coordinator-side
    # floors — placement (or the VM is never disk-eligible for a
    # session) and the idle detector's — in lockstep with the VM-side
    # host-agent idle-evict override below. The two are separate env
    # vars on purpose: prod must be able to tune idle eviction without
    # silently loosening placement's disk gate.
    coord_env['ENGRAM_PLACEMENT_DISK_FLOOR_BYTES'] = '3221225472'
    coord_env['ENGRAM_IDLE_EVICT_DISK_FLOOR_BYTES'] = '3221225472'

if bin_dir:
    # CI: run the downloaded release binary, no compile.
    coord_serve_cmd = 'exec ' + bin_dir + '/engram-coordinator'
elif needs_codesign:
    # VZ (macOS) requires codesigning for host-agent, but the
    # coordinator in split mode doesn't use VZ. We still build it
    # separately but don't codesign since it has no VZ entitlements.
    coord_serve_cmd = (
        'cargo build -p engram-coordinator && ' +
        'exec ./target/debug/engram-coordinator'
    )
else:
    coord_serve_cmd = 'cargo run -p engram-coordinator'

local_resource('coordinator',
    serve_cmd=coord_serve_cmd,
    serve_env=coord_env,
    # Blob setup is intentionally absent: GCS client construction does not
    # contact the emulator or bucket. Keep the control plane available when
    # fake-gcs-server/seed-buckets is unhealthy; only blob-using operations
    # need those resources. Bundles independently gate the host-agent below.
    resource_deps=['postgres', 'registry', 'jaeger'],
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
if two_hosts and fc_colima_profile:
    # ADR 0082 wires exactly one VM-hosted host-agent; a second one would
    # need its own gRPC/metrics ports forwarded out of the SAME VM plus a
    # disjoint NBD/sandbox-dir split inside it, neither of which exists
    # yet. Fail fast instead of silently only starting host-agent.
    fail('ENGRAM_INTEG_TWO_HOSTS is not supported together with ' +
         'ENGRAM_FC_COLIMA_PROFILE yet.')

def _discover_nbd():
    # (ADR 0024) FC serves the chunked rootfs over /dev/nbdN. The host-agent only
    # takes the chunked-NBD path (which produces the chunked disk +
    # memory manifests that `POST /api/enabled-images` REQUIRES — it
    # 500s on a base snapshot built via the materialize-to-file
    # fallback) when ENGRAM_NBD_DEVICES is set, so discover the host's
    # devices and pass them, exactly as the old integration-up.sh did.
    # VZ/macOS don't use NBD.
    if sandbox_backend != 'firecracker':
        return []
    if fc_colima_profile:
        # Devices are in the VM. `colima ssh -- ls /dev/nbd*` does NOT work:
        # colima ssh runs no remote shell, so the glob passes literally,
        # matches nothing, and ENGRAM_NBD_DEVICES ends up unset — which
        # silently drops the host-agent onto the materialize-to-file path,
        # producing base snapshots with no chunked manifest that then fail to
        # restore ("read fc manifest.json … No such file"). List /dev (a lone
        # command) and filter for nbdN on the Mac side.
        listing = str(local(
            "colima ssh --profile " + fc_colima_profile + " -- ls -1 /dev",
            echo_off=True, quiet=True)).strip()
        return ['/dev/' + d.strip() for d in listing.split('\n')
                if d.strip().startswith('nbd') and d.strip()[3:].isdigit()]
    listing = str(local("ls -1 /dev/nbd* 2>/dev/null || true",
                        echo_off=True, quiet=True)).strip()
    return [d for d in listing.split('\n') if d]

_nbd = _discover_nbd()
if two_hosts and len(_nbd) >= 2:
    # Two host-agents on one box need disjoint device sets.
    _half = max(1, len(_nbd) // 2)
    nbd_a = ','.join(_nbd[:_half])
    nbd_b = ','.join(_nbd[_half:])
else:
    nbd_a = ','.join(_nbd)  # single host gets all discovered devices ('' if none)
    nbd_b = None
if sandbox_backend == 'firecracker':
    print('engram dev: NBD devices discovered = %r (two_hosts=%s)' % (_nbd, two_hosts))

def host_agent_resource(name, grpc_port, metrics_port, work_dir, nbd_csv, egress_proxy_port, egress_dns_port):
    env = {
        # `kernel_key` is only non-None for vz/firecracker, both of
        # which force `dev_split`, so this always lands on the
        # host-agent (there's no coord-side mode=all + real-virt path
        # any more — #530 item f made mode=all Process-only).
        kernel_key: kernel_path,
        'ENGRAM_SANDBOX_WORK_DIR': work_dir,
        'ENGRAM_SANDBOX_BACKEND': sandbox_backend,
        # ADR 0061/0055: where the host-agent reads the bundle generation
        # stamp (current.json) + staged <sha>.{erofs,squashfs} files. The
        # `bundles` resource below stages them here before this resource
        # starts (resource_deps). Without this, bundle_dir_from_env() falls
        # back to the Linux fleet path /var/lib/engram/shared (absent on
        # macOS) and every skill-enabled create 400s.
        'ENGRAM_BUNDLE_DIR': _bundle_dir,
        # ADR 0039: the Tilt dev/e2e stack doesn't bake engram-uffd-handler
        # (prod's FC-host image does), and the host-agent runs under sudo
        # with a scrubbed PATH so it couldn't spawn a co-located one anyway.
        # The host-agent code default is now `uffd`, so pin `file` here to
        # keep idle-resume on the no-handler path. Override to `uffd` only
        # where the handler is present. (Base-create stays code-default
        # `file`, which needs no handler.)
        'ENGRAM_FC_RESTORE_MODE': env_or('ENGRAM_FC_RESTORE_MODE', 'file'),
        # ADR 0045 substrate (v2b): point at a tmpfs dir (e.g.
        # /dev/shm/engram) to back Uffd restores with a shared
        # per-template base shm. Empty = off (stock anonymous Uffd).
        # Must be listed here: the host-agent runs under sudo
        # --preserve-env=<these keys>, which scrubs unlisted vars.
        'ENGRAM_FC_UFFD_BASE_DIR': env_or('ENGRAM_FC_UFFD_BASE_DIR', ''),
        # ADR 0092: fresh-create File override + memfile pin + lazy base-shm
        # (all default to today's behavior; the sudo preserve-list is
        # auto-derived from these keys).
        'ENGRAM_FC_FRESH_RESTORE_MODE': env_or('ENGRAM_FC_FRESH_RESTORE_MODE', ''),
        'ENGRAM_FC_BASE_MEMFILE_PIN': env_or('ENGRAM_FC_BASE_MEMFILE_PIN', ''),
        'ENGRAM_FC_BASE_SHM_MODE': env_or('ENGRAM_FC_BASE_SHM_MODE', ''),
        # gRPC plumbing — coord dials advertise, host-agent listens on
        # bind. Same machine in dev, so loopback works for both.
        'ENGRAM_GRPC_LISTEN_ADDR': '127.0.0.1:' + grpc_port,
        'ENGRAM_GRPC_ADVERTISE_ADDR': 'http://127.0.0.1:' + grpc_port,
        # ADR 0045 C2 post-copy page-server listener. Per-host port
        # derived from the gRPC port: the binary default (9102) collides
        # with host-agent-b's gRPC port in the two-host stack, so the
        # coord's gRPC dial lands on the page-server protocol and every
        # restore 503s. +20 keeps it clear of the 910x/911x grpc+metrics
        # block for both hosts.
        'ENGRAM_MIGRATE_PEER_LISTEN_ADDR': '0.0.0.0:' + str(int(grpc_port) + 20),
        'ENGRAM_COORDINATOR_ENDPOINT': 'http://127.0.0.1:8090',
        # Same GCS backend the coord uses, so chunks materialized
        # coord-side are reachable from the PooledBackend at runtime.
        'ENGRAM_BLOB_BACKEND': 'gcs',
        'ENGRAM_GCS_BUCKET': env_or('ENGRAM_GCS_BUCKET', 'engram-snapshots-test'),
        'STORAGE_EMULATOR_HOST': env_or('STORAGE_EMULATOR_HOST', 'http://localhost:4443'),
        # Egress proxy port. The proxy is MANDATORY (issue #240) — the only
        # path a guest reaches the network (SNI allow-list + DNS filter);
        # there's no "off" (0 is a footgun: a dead :443->0 redirect while the
        # proxy binds a random port). Per-host fixed port (each host-agent
        # binds 0.0.0.0:<port> + iptables REDIRECTs its VMs there). The FC path
        # defaults this to 8443 (see _proxy_base below) so claude sessions can
        # reach api.anthropic.com; a session still needs api.anthropic.com in
        # its policy network.allow_hosts + ANTHROPIC_API_KEY for the call to
        # succeed (ADR 0006/0057).
        'ENGRAM_EGRESS_PROXY_PORT': egress_proxy_port,
        # DNS-filter proxy port. Like the proxy/gRPC/metrics ports, it must be
        # distinct per host-agent on the SHARED netns of the two-host e2e stack
        # (host-agent-b gets dns_base+1 below) — else the second host-agent
        # fails closed on `Address already in use` (ADR 0083). The
        # host-agent wires this same value into the FC iptables `:53 -> dns`
        # REDIRECT, so the two can't drift.
        'ENGRAM_EGRESS_DNS_PORT': egress_dns_port,
        'ENGRAM_HOST_METRICS_ADDR': '0.0.0.0:' + metrics_port,
        # ADR 0019: same OTLP target as the coord, so the host-side
        # restore/boot spans land in the same Jaeger trace.
        'OTEL_EXPORTER_OTLP_ENDPOINT': otel_endpoint,
        'RUST_LOG': 'info,engram=debug',
    }
    # ADR 0045 substrate (v2b): dev uffd runs spawn the workspace-built
    # handler (prod bakes it onto PATH). Conditional — an empty env var
    # would clobber the host-agent's PATH-lookup default.
    if env_or('ENGRAM_FC_UFFD_HANDLER_BIN', ''):
        env['ENGRAM_FC_UFFD_HANDLER_BIN'] = env_or('ENGRAM_FC_UFFD_HANDLER_BIN', '')

    if nbd_csv:
        env['ENGRAM_NBD_DEVICES'] = nbd_csv

    if fc_colima_profile:
        # ADR 0082: this resource's whole execution model moves into the
        # VM — addresses, PATH, and the build/sync/serve steps below all
        # target it, not the Mac. Only the two addresses below actually
        # need to change: Mac->VM (gRPC) and VM->Mac (coordinator) keep
        # their loopback semantics via Lima's auto-forward + the gateway,
        # per the ADR's networking section.
        env['ENGRAM_SANDBOX_WORK_DIR'] = '/opt/engram-dev/var/sandboxes'
        env['ENGRAM_BUNDLE_DIR'] = '/opt/engram-dev/shared'
        env['ENGRAM_GRPC_LISTEN_ADDR'] = '0.0.0.0:' + grpc_port
        env['ENGRAM_COORDINATOR_ENDPOINT'] = 'http://192.168.5.2:8090'
        env['ENGRAM_FIRECRACKER_BIN'] = '/usr/local/bin/firecracker'
        # ADR 0080: enable-time materialization runs inside the VM-side
        # host-agent. Provisioning installs a stable mke2fs contract path
        # there so sudo/PATH drift cannot drop the ext4 packer.
        env['ENGRAM_MKE2FS'] = '/opt/engram-dev/bin/mke2fs'
        # We always cross-compile + sync engram-uffd-handler alongside
        # engram-host-agent (below), so point at it explicitly rather than
        # rely on the VM's PATH — ADR 0045's Uffd restore path becomes
        # exercisable here for the first time (still opt-in via
        # ENGRAM_FC_RESTORE_MODE=uffd; the dev default stays `file`).
        env['ENGRAM_FC_UFFD_HANDLER_BIN'] = '/opt/engram-dev/bin/engram-uffd-handler'
        # The idle-evict disk-pressure floor defaults to 20 GiB — LARGER than
        # the fc-dev VM's ~19 GiB rootfs, so free disk can never exceed it and
        # idle eviction would be permanently paused ("disk pressure (free <
        # floor)"), never reclaiming space on a small dev VM. Drop it to 3 GiB
        # so eviction actually runs and keeps the VM from filling.
        env['ENGRAM_IDLE_EVICT_DISK_FLOOR_BYTES'] = '3221225472'
        # No KEK / egress-CA material crosses the VM boundary: the
        # host-agent never reads ENGRAM_KEK_MASTER_KEY (coordinator/
        # orchestrator only — grep confirms no reference in
        # crates/engram-host-agent), and its egress CA `--ca-source`
        # defaults to `local-disk`, which self-generates a CA under
        # `<work_dir>/egress-ca` on first boot (see `build_host_egress` in
        # crates/engram-host-agent/src/main.rs) — nothing to sync there
        # either. The mandatory egress proxy uses the FC fixed ports selected
        # below; local-disk CA generation stays entirely inside the VM.
        env['PATH'] = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
    elif 'Darwin' in uname_str:
        env['PATH'] = '/opt/homebrew/opt/e2fsprogs/sbin:' + os.environ.get('PATH', '')
        # ADR 0080 §C: the enable-time materializer packs ext4 via mke2fs.
        # Point it at homebrew's keg-only e2fsprogs (>= 1.47.1) explicitly so
        # resolution never depends on PATH ordering — the dev mirror of the
        # host-agent image's ENGRAM_MKE2FS=/usr/sbin/mke2fs.
        env['ENGRAM_MKE2FS'] = '/opt/homebrew/opt/e2fsprogs/sbin/mke2fs'
    else:
        # Linux: the host-agent runs under sudo (below), which scrubs
        # PATH to a secure default. Preserve the caller's PATH so it
        # still finds firecracker / ip / iptables / mke2fs.
        env['PATH'] = os.environ.get('PATH', '')

    if fc_colima_profile:
        # Cross-compile on the Mac (native cargo can't target the VM's
        # aarch64-linux triple any other way here), then push the two
        # binaries + the staged bundle dir onto the VM's OWN disk over
        # `colima ssh` — FC serves bundle files as block-device backings,
        # which must never be read off the virtiofs mount. There's no
        # `colima cp`, so tar-over-ssh does both syncs (mirrors how the
        # `bundles` resource below already stages var/shared on the Mac
        # side first).
        target = 'aarch64-unknown-linux-musl'
        # Tilt's serve process runs with a reduced PATH that (unlike the
        # parse-time local() context and an interactive shell) does NOT
        # include Homebrew's bin dir, so a bare `colima` fails with
        # "command not found". Resolve it to an absolute path once at the
        # front of the serve_cmd and reference "$_colima" everywhere — the
        # var propagates across the &&/| stages since they share one shell.
        colima = '"$_colima"'
        # Cross-compile under a real `nix develop` shell: cc-rs building ring's
        # C code needs the exact per-target CC/CFLAGS `nix develop` sets up (a
        # sourced env snapshot got the wrong musl-gcc and leaked Apple clang
        # flags like `-arch arm64` into the aarch64 build). host-agent rebuilds
        # rarely (only on a .rs change or a current.json repoint), so the
        # per-build flake eval is a non-issue.
        build_cmd = (
            _nix + ' develop -c cargo build --release --target ' + target +
            ' -p engram-host-agent -p engram-uffd-handler'
        )
        # NOTE on remote-command quoting: `colima ssh -- <args>` does NOT run
        # the joined args through a remote shell, so `&&`/`||`/`|` in a single
        # remote arg are taken literally ("command not found"). Keep each
        # remote command a single program (no shell operators), or wrap it in
        # an explicit `bash -c '...'` (see prekill below). The mkdir uses the
        # OUTER Mac shell's `&&` (fine); the remote `tar xf` is a lone command.
        # No remote chmod needed — tar preserves the source's +x bit.
        # Keep macOS tar cruft out of the stream: COPYFILE_DISABLE=1 drops the
        # ._* AppleDouble companion files, and --no-xattrs drops the
        # com.apple.* xattrs that macOS bsdtar otherwise embeds as PAX headers
        # (LIBARCHIVE.xattr.*), which the VM's GNU tar spams "Ignoring unknown
        # extended header keyword" over on extract. Neither is wanted in-guest.
        sync_bin_cmd = (
            colima + ' ssh --profile ' + fc_colima_profile +
            ' -- mkdir -p /opt/engram-dev/bin /opt/engram-dev/var/sandboxes && ' +
            'COPYFILE_DISABLE=1 tar --no-xattrs -cf - -C target/' + target + '/release ' +
            'engram-host-agent engram-uffd-handler | ' +
            colima + ' ssh --profile ' + fc_colima_profile +
            ' -- tar xf - -C /opt/engram-dev/bin'
        )
        # Sync ONLY current.json + the squashfs it references — NOT all of
        # var/shared. That dir also accumulates stale generations and (from
        # VZ `just dev` runs) large .erofs bundles the FC guest can't even
        # mount; `tar -C var/shared .` copied all of it (~9 GiB of erofs) into
        # the VM and filled its disk. Derive the file list from current.json's
        # shas at runtime. (/opt/engram-dev/shared is pre-created by
        # fc-colima-provision.sh, so the remote side is a lone `tar xf`.)
        sync_bundle_cmd = (
            'COPYFILE_DISABLE=1 tar --no-xattrs -cf - -C ' + _bundle_dir +
            " current.json $(grep -oE '[0-9a-f]{64}' " + _bundle_dir +
            "/current.json | sed 's/$/.squashfs/') | " +
            colima + ' ssh --profile ' + fc_colima_profile +
            ' -- tar xf - -C /opt/engram-dev/shared'
        )
        # Restart semantics: verified live that `colima ssh`'s multiplexed
        # ControlMaster transport does NOT propagate SIGTERM/SIGHUP to the
        # remote process when Tilt kills this local serve_cmd — killing
        # the local `colima ssh` wrapper (even as a whole process group)
        # leaves the remote engram-host-agent running. colima ssh exposes
        # no `-t`/pty flag to fix this from here, so the pre-kill below is
        # the actual mechanism, not just a backstop: every (re)start kills
        # any prior instance before launching a new one. One consequence:
        # `tilt down` (or Tilt exiting) does NOT stop the remote
        # host-agent or its live microVMs — they keep running in the VM
        # until the next `tilt up` / `dev-fc`, or a manual
        # `colima ssh --profile <p> -- sudo pkill -f engram-host-agent`.
        # Keep the REMOTE side a lone command (`sudo -n pkill -f <bin>`, no
        # operators/quotes — `bash -c '...'` quoting doesn't survive colima ssh
        # reliably) and put the `|| true` on the Mac side, in a subshell so it
        # only swallows pkill's "no match" exit (not the &&-chained syncs).
        prekill_cmd = (
            '( ' + colima + ' ssh --profile ' + fc_colima_profile +
            ' -- sudo -n pkill -f /opt/engram-dev/bin/engram-host-agent || true )'
        )
        env_kv = ' '.join([k + '=' + v for k, v in env.items()])
        serve_cmd = (
            # Tilt's serve process has a reduced PATH without Homebrew's bin
            # dir. `colima` itself shells out to `limactl` (both in
            # /opt/homebrew/bin), so an absolute `colima` path isn't enough —
            # put Homebrew on PATH so colima finds lima. Safe for the
            # `nix develop -c` cross-build below: nix prepends its own toolchain
            # ahead of this (matches the spike env that built the binaries).
            'export PATH="/opt/homebrew/bin:$PATH" && ' +
            '_colima="$(command -v colima || echo /opt/homebrew/bin/colima)" && ' +
            build_cmd + ' && ' + sync_bin_cmd + ' && ' + sync_bundle_cmd +
            ' && ' + prekill_cmd + ' && ' +
            'exec ' + colima + ' ssh --profile ' + fc_colima_profile +
            ' -- sudo -n env ' + env_kv +
            ' /opt/engram-dev/bin/engram-host-agent'
        )
    else:
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
                'bash crates/engram-sandbox-vz/scripts/codesign.sh debug && ' +
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
        resource_deps=['coordinator', 'bundles'],
        readiness_probe=probe(
            period_secs=30,
            timeout_secs=2,
            tcp_socket=tcp_socket_action(port=int(grpc_port)),
        ),
        links=[
            link('http://127.0.0.1:' + metrics_port + '/metrics', 'metrics'),
        ],
        labels=['app'],
        # ADR 0061: AUTO so a bundle rebuild (current.json change, in `deps`
        # below) auto-restarts the host-agent → it re-reads the stamp and a
        # new session runs the edited skill. Source edits are NOT in `deps`,
        # so a .rs change still requires a manual trigger (unchanged loop).
        trigger_mode=TRIGGER_MODE_AUTO,
        deps=[_bundle_dir + '/current.json'],
        auto_init=True)

# ADR 0061 / 0055: stage the skill bundles (content-addressed payloads +
# a current.json stamp) under var/shared BEFORE the host-agent boots, so
# it reports `current_bundles` and the coordinator can resolve enabled
# skills (else `POST /sessions` 400s with "skill `skills` is unknown").
# The fs format follows the backend: VZ's Kata kernel mounts erofs, FC
# mounts squashfs. Re-running this (a skill edit under deploy/bundles)
# rewrites current.json, which restarts the host-agent (its `deps` below)
# so it re-reads the stamp — new sessions pick the edit up.
_bundle_dir = os.path.abspath('var/shared')
if dev_split:
    _bundles_recipe = 'bundles-vz' if sandbox_backend == 'vz' else 'bundles-squashfs'
    _bundles_cmd = 'just ' + _bundles_recipe
    # On the fc-colima path this resource auto-retriggered forever. The build
    # is content-deterministic and never modifies deploy/bundles (verified:
    # content + mtime unchanged across builds), but the Docker-built bundles
    # under colima's file-sharing bump the *ctime* of the whole deploy/bundles
    # tree as a side effect, and Tilt's deps watcher fires on ctime -> rebuild
    # -> ctime sweep -> loop. (VZ dev never hit it — no fc-dev colima VM /
    # docker-bundle churn in that path.) The build's output is deterministic,
    # so losing auto-rebuild costs nothing: build once at startup, and after an
    # intentional skill edit re-trigger `bundles` from the Tilt UI. Keep AUTO
    # off the fc path so plain `just dev` (VZ) still auto-rebuilds on edits.
    _bundles_trigger = TRIGGER_MODE_AUTO
    if fc_colima_profile and 'Darwin' in uname_str:
        # Run under a real `nix develop`: bundles-squashfs needs mksquashfs AND
        # (to build the harness-claude bundle locally, ADR 0062) the aarch64
        # musl cross-toolchain with the exact per-target CC/CFLAGS — the sourced
        # print-dev-env snapshot got the cc-rs cross-build wrong. `nix develop`
        # keeps the inherited PATH, so prepend Homebrew's bin so the Docker-built
        # bundles find docker/colima. TRIGGER_MODE_MANUAL (below) is what stops
        # the rebuild loop, so paying the per-build flake eval once is fine.
        _bundles_cmd = ('export PATH="/opt/homebrew/bin:$PATH" && ' + _nix +
                        ' develop -c ' + _bundles_cmd)
        # The loop: the Docker-built bundles under colima's file-sharing bump
        # the *ctime* of the whole deploy/bundles tree, and Tilt's deps watcher
        # fires on ctime -> rebuild -> ctime sweep -> forever. (VZ never hit it —
        # no fc-dev colima VM / docker-bundle churn.) The build output is
        # deterministic, so MANUAL costs nothing: build once at startup, and
        # re-trigger `bundles` from the Tilt UI after an intentional skill edit.
        _bundles_trigger = TRIGGER_MODE_MANUAL
    local_resource('bundles',
        cmd=_bundles_cmd,
        deps=['deploy/bundles'],
        trigger_mode=_bundles_trigger,
        labels=['setup'])

# The egress proxy is MANDATORY (issue #240): it's the only path a guest
# reaches the network, and `0` is NOT an "off" sentinel — host_startup still
# installs a `:443 -> port 0` REDIRECT (dead) while the proxy binds a random
# ephemeral port, so guest HTTPS (e.g. a claude session's api.anthropic.com
# call) silently hangs. Every dev backend gets real fixed ports. FC keeps the
# binary defaults because iptables redirects its guests there. Local VZ uses a
# separate pair: Colima's SSH forwarding can retain the FC ports on the Mac,
# and UDP 5353 is the system mDNS port. Explicit env overrides still win.
_proxy_default = '8443' if sandbox_backend == 'firecracker' else '18443'
_dns_default = '5353' if sandbox_backend == 'firecracker' else '15353'
_proxy_base = int(env_or('ENGRAM_EGRESS_PROXY_PORT', _proxy_default))
_dns_base = int(env_or('ENGRAM_EGRESS_DNS_PORT', _dns_default))
# host-agent-b takes the next proxy and DNS ports so the two hosts don't
# collide on the shared netns.
if _proxy_base < 1 or _proxy_base > 65535:
    fail('ENGRAM_EGRESS_PROXY_PORT must be between 1 and 65535 (egress is mandatory)')
if _dns_base < 1 or _dns_base > 65535:
    fail('ENGRAM_EGRESS_DNS_PORT must be between 1 and 65535 (egress is mandatory)')
if two_hosts and (_proxy_base == 65535 or _dns_base == 65535):
    fail('two-host mode needs room for the second host egress ports (base must be <= 65534)')
if dev_split:
    host_agent_resource('host-agent', '9101', '9100', './var/host-sandboxes', nbd_a, str(_proxy_base), str(_dns_base))
    if fc_colima_profile:
        # ADR 0082: the coordinator dials the host-agent's advertised
        # 127.0.0.1:9101 (and scrapes metrics on :9100) — reachable only via a
        # guest->Mac forward. Lima's auto-forward is edge-triggered and proved
        # unreliable across host-agent/VM restarts (the port silently stops
        # forwarding -> "no host could restore … tcp connect error"). Hold the
        # forward DETERMINISTICALLY ourselves with `ssh -L` over colima's own
        # ssh (regenerate ssh-config each start — the VM's ssh port changes on
        # restart; ServerAliveInterval drops the tunnel when the VM dies so
        # Tilt restarts + reconnects). This is what makes coord->host stable.
        local_resource('fc-grpc-forward',
            serve_cmd=(
                'export PATH="/opt/homebrew/bin:$PATH" && ' +
                'cfg="$(mktemp)" && colima ssh-config ' + fc_colima_profile +
                ' > "$cfg" && ' +
                # Drop any forwards a prior colima mux master still holds on
                # 9101/9100 (see ControlPath=none note below). `-O cancel`
                # removes just those forwards WITHOUT killing the master, so the
                # host-agent's own colima-ssh session riding that master stays
                # up. No-op once nothing forwards over the shared master.
                '( ssh -F "$cfg" -O cancel ' +
                '-L 127.0.0.1:9101:127.0.0.1:9101 ' +
                '-L 127.0.0.1:9100:127.0.0.1:9100 ' +
                'colima-' + fc_colima_profile + ' 2>/dev/null || true ) && ' +
                'exec ssh -F "$cfg" -N ' +
                # colima's ssh-config sets `ControlMaster auto` + `ControlPersist
                # yes`. Left as-is, our `-N` forward would set up the shared
                # master, then ControlPersist DAEMONIZES it into the background
                # and the foreground ssh returns 0 immediately — Tilt sees the
                # serve_cmd "exit 0", flags the resource dead, and the retry
                # then trips ExitOnForwardFailure (the backgrounded master still
                # owns the ports). Force a dedicated, non-multiplexed connection
                # so `-N` blocks HERE in the foreground where Tilt supervises it
                # and it dies cleanly on kill (verified: default cfg → exit 0;
                # ControlPath=none → blocks). Must be BEFORE the -L flags.
                '-o ControlMaster=no -o ControlPath=none ' +
                '-o ExitOnForwardFailure=yes -o ServerAliveInterval=5 ' +
                '-o ServerAliveCountMax=3 ' +
                '-L 127.0.0.1:9101:127.0.0.1:9101 ' +
                '-L 127.0.0.1:9100:127.0.0.1:9100 ' +
                'colima-' + fc_colima_profile),
            resource_deps=['host-agent'],
            labels=['setup'])
    if two_hosts:
        # Distinct proxy + DNS ports for the second host-agent (they share the
        # netns), else host-agent-b fails closed on Address already in use
        # (ADR 0083).
        _proxy_b = str(_proxy_base + 1)
        host_agent_resource('host-agent-b', '9102', '9110', './var/host-sandboxes-b', nbd_b, _proxy_b, str(_dns_base + 1))

# ----------------------------------------------------------------
# Orchestrator (Bun/Hono, ADR 0051) — the web's BFF.
#
# The web's vite proxy sends all /rpc + /api to the orchestrator on
# :8787; the orchestrator owns better-auth sessions + per-user secrets
# and proxies into the coordinator's app-gRPC (CONTROL_PLANE_GRPC_URL)
# and REST (CONTROL_PLANE_HTTP_URL). So whenever the web runs, the
# orchestrator must run too (same `if not skip_web` gate below).
#
# `orchestrator-migrate` is a one-shot: it creates the orchestrator DB
# if the postgres volume pre-existed initdb (the docker-entrypoint
# initdb.d script only runs on a FRESH volume) and applies the drizzle
# migrations idempotently. The `orchestrator` serve resource depends on
# it so the schema (incl. user_session_secrets) exists before boot.
# ----------------------------------------------------------------

orchestrator_db_url = 'postgres://engram:engram@localhost:5435/engram_orchestrator'

# CONTROL_PLANE_BEARER MUST match the coordinator's accepted app-gRPC
# token (ENGRAM_APP_GRPC_TOKENS in coord_env above), else the coord
# fails the orchestrator's RPCs closed.
orchestrator_env = {
    'ORCHESTRATOR_DATABASE_URL': orchestrator_db_url,
    'CONTROL_PLANE_BEARER': env_or('ENGRAM_APP_GRPC_TOKENS', 'dev-app-grpc-token'),
    'CONTROL_PLANE_GRPC_URL': 'http://127.0.0.1:50061',
    'CONTROL_PLANE_HTTP_URL': 'http://127.0.0.1:8090',
    'ORCHESTRATOR_PORT': '8787',
    # Public origin the BROWSER uses (the web dev server) — NOT the orchestrator's
    # own :8787. better-auth's session cookie is scoped here, /api is proxied here
    # (vite.config.ts), and the OAuth redirect + Slack session links are built from
    # it. Unset → it falls back to 127.0.0.1:8787, so the OAuth callback bypasses the
    # proxy and arrives cookie-less → 401. Override with an https tunnel URL (ngrok/
    # cloudflared) for real Slack OAuth, which rejects non-https redirect URLs.
    'ORCHESTRATOR_PUBLIC_URL': env_or('ORCHESTRATOR_PUBLIC_URL', 'http://localhost:5173'),
    'TRUSTED_ORIGINS': 'http://localhost:5173',
    # Dev-only better-auth signing secret (≥32 chars). better-auth 1.6.16
    # silently falls back to a publicly-known constant when unset, so the
    # orchestrator requires it; a fixed dev literal is fine locally but
    # MUST be rotated for any real deploy.
    'BETTER_AUTH_SECRET': 'engram-dev-only-better-auth-secret-do-not-use-in-prod',
    # SAME KEK the coordinator uses (coord_env above), so user_session_secrets
    # sealed by either side are format-identical. `just bootstrap` writes it
    # to .env at parse time (above). Required — the orchestrator refuses to
    # boot without it, which is correct: a missing .env KEK is a real misconfig.
    'ENGRAM_KEK_MASTER_KEY': env_or('ENGRAM_KEK_MASTER_KEY', ''),
    # ADR 0089: register the dev-only smoke tools (dev_echo / dev_echo_deferred)
    # so live scenario A/C verification works against the local stack.
    'ENGRAM_DEV_TOOLS': '1',
}

skip_web = env_or('ENGRAM_SKIP_WEB', '') in ('1', 'true', 'yes')

# The orchestrator is a CORE stack member (not web-gated): it is the product
# API tier the `engrams` CLI drives, so every dev/CI flow — including the
# ENGRAM_SKIP_WEB=1 e2e/integration stacks — needs it up. Only the vite SPA
# stays behind skip_web.

# createdb returns nonzero if the DB already exists (initdb made it on a
# fresh volume); swallow that and let drizzle-kit migrate carry the schema.
local_resource('orchestrator-migrate',
    cmd=(
        'cd orchestrator && bun install --silent && ' +
        '(PGPASSWORD=engram createdb -h localhost -p 5435 -U engram ' +
        'engram_orchestrator 2>/dev/null || true) && ' +
        'ORCHESTRATOR_DATABASE_URL=' + orchestrator_db_url + ' ' +
        'bunx drizzle-kit migrate'
    ),
    resource_deps=['postgres'],
    labels=['setup'])

local_resource('orchestrator',
    serve_cmd='cd orchestrator && bun install --silent && bun run start',
    serve_env=orchestrator_env,
    resource_deps=['postgres', 'orchestrator-migrate', 'coordinator'],
    readiness_probe=probe(
        period_secs=30,
        timeout_secs=2,
        tcp_socket=tcp_socket_action(port=8787),
    ),
    links=[
        link('http://127.0.0.1:8787/healthz', 'healthz'),
    ],
    labels=['app'],
    trigger_mode=TRIGGER_MODE_MANUAL,
    auto_init=True)

# Seed the headless dev credential the `engrams` CLI (justfile recipes,
# deploy/dev scripts, CI) authenticates with: an admin service-account API
# key whose plaintext lands in var/dev-api-key (gitignored, 0600).
# Idempotent — re-mints only when the file is missing or the key row is gone.
local_resource('dev-api-key',
    cmd=(
        'cd orchestrator && ' +
        'ORCHESTRATOR_DATABASE_URL=' + orchestrator_db_url + ' ' +
        'BETTER_AUTH_SECRET="' + orchestrator_env['BETTER_AUTH_SECRET'] + '" ' +
        'CONTROL_PLANE_BEARER="' + orchestrator_env['CONTROL_PLANE_BEARER'] + '" ' +
        'ENGRAM_KEK_MASTER_KEY="' + orchestrator_env['ENGRAM_KEK_MASTER_KEY'] + '" ' +
        'bun scripts/seed-dev-key.ts ../var/dev-api-key'
    ),
    resource_deps=['orchestrator-migrate'],
    labels=['setup'])

# ----------------------------------------------------------------
# Web SPA (vite dev server).
#
# Vite's HMR runs in-process — Tilt should NEVER restart this.
# Edits to web/src/** are picked up via vite's own file watcher,
# not via a Tilt re-run. We deliberately don't pass `deps` here.
#
# The vite proxy now targets the orchestrator (:8787) for /rpc + /api,
# so the web depends on `orchestrator` (not the coordinator directly).
#
# Skipped when ENGRAM_SKIP_WEB is set (CI's `tilt ci` has no browser
# to drive the SPA, and pnpm install + a vite readiness wait only slow
# the e2e gate down). Mirrors integration-up.sh's ENGRAM_SKIP_WEB.
# ----------------------------------------------------------------

if not skip_web:
    local_resource('web',
        serve_cmd='cd web && pnpm install --silent && pnpm dev --strictPort',
        resource_deps=['orchestrator'],
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
# infra (postgres, registry) → setup (seed-buckets, orchestrator-migrate)
# → app (coordinator, host-agent, orchestrator, web). Click any one to
# jump to its log stream / readiness state / restart button.
