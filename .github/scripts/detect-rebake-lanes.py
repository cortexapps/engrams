#!/usr/bin/env python3
"""Decide which deploy lanes a change set affects, from the cargo dep graph.

Replaces the hand-maintained path denylist in bake-images.yml's `detect`
job (which silently drifts: a crate that host-agent/uffd-handler actually
depends on can get wrongly denied, shipping a stale FC binary).

A lane is affected iff a changed file maps to a crate in that lane's
binary **release** dependency closure (dev-deps excluded — they don't
ship), or a changed file matches a non-crate path rule for the lane.

Lanes:
  images        container rebake — release closure of {coordinator, host-agent}
                + web/ + orchestrator/ + docker/ + migrations/
  host_binaries FC host binaries changed — release closure of
                {host-agent, uffd-handler} + rust-toolchain.toml. Drives the
                fast per-commit THIN bake (just re-drop the two binaries onto
                the base image).
  host_base     FC host OS layer changed — deploy/packer/ + deploy/otel/.
                Drives the infrequent BASE bake (apt, Firecracker, kernel,
                Ops Agent, otelcol, systemd unit). A base rebake re-triggers a
                thin bake downstream so the new OS layer reaches prod.
  host_image    host_binaries OR host_base. The union gates the OSS
                publish-host-binaries job — the GHCR artifact must exist at
                this SHA for either downstream bake to consume.
  cli_tools     the `engrams` CLI changed — cli/ sources or the protos it
                codegens from (no cargo closure: the CLI is Bun/TS since the
                orchestrator-native rewrite). Gates the OSS publish-cli-tools
                job, which republishes the "golden" cli GHCR artifact
                (cli-tools) that CI consumers pull instead of recompiling.
                Same role publish-host-binaries plays for the FC-host bakes.
  node_assets   the node-assets image (firecracker + guest kernel + the RO
                session bundles) changed — docker/node-assets.Dockerfile /
                docker/node-assets-fetch.sh (the FC/kernel pins + bundle staging)
                OR the fc_fork lane OR the bundles lane. Gates the OSS
                publish-node-assets job. DELIBERATELY NARROWER than `images`: a
                coordinator/web/orchestrator change trips `images` (the container
                bake) but touches NONE of node-assets' inputs, so it must NOT
                rebake node-assets — a spurious rebake churns the SHA tag the
                deploy pins into the host-fleet DaemonSet and rolls every FC host
                for nothing (2026-07-20 incident: four host rolls in 90 min off
                three coord/web-only pushes).
  tf_or_helm    deploy/terraform/ + deploy/helm/

(The dev-engrams dogfood image is no longer a lane here: engrams-internal
bakes + refreshes it on a workday-hourly schedule against OSS main — the same
model as its dev-brain image — so the `engrams-dev-image-changed` dispatch is
retired.)

`Cargo.lock` / root `Cargo.toml` / this script / the bake workflow are
conservative triggers for the host_binaries lane.

Usage (CI):   detect-rebake-lanes.py --base "$BEFORE" --head "$HEAD"
Usage (test): printf 'crates/engram-postgres/src/x.rs\n' | detect-rebake-lanes.py --stdin
"""
import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

# Binaries that bake into each artifact.
FC_BINS = {"engram-host-agent", "engram-uffd-handler"}

# ADR 0098 P9: the host-internal simulation lane. Gated on the release
# closure of the sim binary's own crate — engram-dst-host pulls
# engram-host-agent + engram-host-core + engram-sim + the chunk/storage
# crates as NORMAL deps, so any change that can alter host-sim behavior
# trips the lane, and nothing else does (disjoint from engram-dst's
# coordinator closure by construction — the two sims share no sim crate
# dep direction).
HOST_SIM_BINS = {"engram-dst-host"}

# ADR 0098 R-CoSim (rung 1): the coordinator↔host BOUNDARY simulator. Unlike
# the two disjoint sims above, engram-dst-cosim deliberately spans BOTH
# closures — it pulls engram-coordinator AND engram-host-agent/-core +
# engram-dst-host + engram-sim as normal deps — so any change that can alter
# either side's boundary behavior trips this lane. Its own release closure
# is therefore the union that gates `test-cosim`.
COSIM_BINS = {"engram-dst-cosim"}
# engram-host-operator (ADR 0044 K3) bakes into its own container image; add
# it so an operator-only crate change rebuilds the images lane.
#
# engram-uffd-handler: since ADR 0044 the host fleet is a K8s DaemonSet and
# the handler ships INSIDE the host-agent container
# (docker/host-agent.Dockerfile copies it to /usr/local/bin) — it is NOT a
# cargo dependency of host-agent (a sibling binary spawned by path), so
# without listing it here a handler-only change set images=False and landed
# only in the dead GCE host_binaries lane: it deployed NOWHERE (PR #193's
# handler fix sat unrolled until this was caught).
CONTAINER_BINS = {
    "engram-coordinator",
    "engram-host-agent",
    "engram-host-operator",
    "engram-uffd-handler",
}
# The built-in claude harness binary. ADR 0062: it is NOT baked into any
# image — it rides the fleet `current_bundles` stamp (the
# `bake-harness-claude-artifact` job publishes the harness-claude artifact;
# node-assets stages it onto hosts). A change to the harness binary (or
# anything in its release closure) must republish that artifact. It is in
# NEITHER the container nor the host closure, so without this lane a
# harness-only change shipped nothing (the bug this gate fixes).
SESSION_HARNESS_BINS = {"engram-harness-claude", "engram-harness-codex"}
HARNESS_PATHS = ["deploy/harness-claude/", "deploy/harness-codex/"]
# The product CLI (`engrams`, cli/ — Bun/TS, orchestrator-native; the Rust
# engram-cli crate is retired). No cargo closure: its inputs are its own
# sources + the generated proto bindings. OSS publishes it once as the
# `cli-tools` GHCR artifact (publish-cli-tools); CI consumers pull that
# instead of recompiling — exactly how the FC-host bakes consume
# publish-host-binaries.
CLI_PATHS = ["cli/"]
# ADR 0080: the in-guest agent, exec'd out of its reserved bundle slot by the
# stage-1 init. A change to it (or its release closure) must republish
# `bundle-agentd` via publish-bundles — the identical coupling (and failure
# mode) as the harness: roll the fleet against a stale agentd bundle and every
# fresh create runs yesterday's agentd.
AGENTD_BINS = {"engram-agentd"}
# The binaries the `test-e2e-stack` lane builds + boots: coord + host-agent
# (the stack), agentd (staged into the host bundle stamp; ADR 0080), and
# harness-claude (likewise; ADR 0062). The `engrams` CLI (which drives
# enable/registry in that lane) is not a crate — it rides E2E_PATHS via
# `cli/`. A change anywhere in this closure means the e2e lane could behave
# differently, so run it. Gates the expensive e2e stack lane.
E2E_BINS = {
    "engram-coordinator",
    "engram-host-agent",
    "engram-agentd",
    "engram-harness-claude",
    "engram-harness-codex",
}

# Non-crate path prefixes per lane. `Cargo.lock`/root `Cargo.toml`/this
# script/the bake workflow conservatively trip the binary lanes.
BINARY_COMMON = ["Cargo.lock", "Cargo.toml",
                 ".github/workflows/bake-images.yml",
                 ".github/scripts/detect-rebake-lanes.py"]
# A toolchain bump changes how the binaries compile, so it belongs to the
# binaries lane (recompile + republish), not the OS-layer base lane.
HOST_BINARIES_PATHS = ["rust-toolchain.toml"] + BINARY_COMMON
# The OS layer baked into engram-fc-host-base: the packer manifests +
# provisioners (deploy/packer/) and the otelcol config (deploy/otel/). A
# change here means the BASE image must be re-baked. (deploy/otel/ was missing
# from the old single host_image lane — an otel-config change silently never
# rebaked the host.)
HOST_BASE_PATHS = ["deploy/packer/", "deploy/otel/"]
# `orchestrator/` (ADR 0051): the Bun/Hono orchestrator builds its own
# container image (docker/orchestrator.Dockerfile) just like `web/` — it has no
# crate in the cargo graph, so it rides the `images` lane via this path rule,
# exactly mirroring `web/`'s treatment. A source-only orchestrator change
# rebakes the orchestrator (and the other images in the matrix; that's the same
# coarse behavior `web/` has always had).
IMAGES_PATHS = ["docker/", "web/", "orchestrator/", "deploy/migrations/"] + BINARY_COMMON
TF_HELM_PATHS = ["deploy/terraform/", "deploy/helm/"]
# ADR 0027: the RO session bundles (skills / browser / …). A change here means
# the bundle artifacts must be rebuilt + republished, and the FC-host image
# re-baked to pull the new squashfs. Independent of the Rust/OS lanes.
# ADR 0080 §D: the `guest-tools` bundle (the pinned static ttyd) lives entirely
# under deploy/bundles/guest-tools/ (a pinned download in build.sh — no crate),
# so `bundles |= guest_tools_changed` is already covered by this path rule; a
# ttyd re-pin trips `bundles` and republishes bundle-guest-tools. (Contrast
# agentd, a compiled crate OUTSIDE this path, which needs the explicit
# agentd_changed closure term below.)
BUNDLES_PATHS = ["deploy/bundles/"]
# The node-assets image's OWN inputs (ADR 0044 K2): its Dockerfile and the fetch
# script that pins the firecracker version + the engram guest-kernel release +
# stages the RO bundles. The FC-fork binary (fc_fork lane) and the bundle
# payloads (bundles lane) are folded into `node_assets` below. This is
# INTENTIONALLY disjoint from a coordinator/web/orchestrator source change: those
# trip `images` (rebake the container) but change nothing the node-assets image
# carries, so they must not rebake it (a rebake churns the DaemonSet tag and
# rolls the fleet for nothing — the 2026-07-20 host-roll-churn incident).
NODE_ASSETS_PATHS = ["docker/node-assets.Dockerfile", "docker/node-assets-fetch.sh"]
# ADR 0045 Phase B: the vendored Firecracker fork (a submodule + the `.gitmodules`
# gitlink). Bumping the submodule pointer (the daily auto-rebase, or a manual
# port) changes the FC *binary* the node-assets image stages, so it must rebuild
# node-assets + roll the host fleet — folded into the `images` lane below.
# Inert until the submodule exists (these paths don't change today).
FC_FORK_PATHS = ["third_party/firecracker", ".gitmodules"]
# Non-crate inputs to every `test-e2e-stack` variant: the dev-orchestration
# scripts + Tiltfile that bring the stack up, the demo image sources it
# bakes, the RO bundles it stages, the CLI used for stack setup, and the
# workflow / detector themselves.
# (The e2e test file lives under crates/engram-coordinator/, so it's already
# covered by that crate being in E2E_BINS' closure.)
E2E_PATHS = [
    "deploy/dev/",
    "Tiltfile",
    "deploy/demo/",
    "deploy/bundles/",
    "cli/",  # the `engrams` CLI drives enable/registry/session in the lane
    ".github/workflows/ci.yml",
    ".github/scripts/detect-rebake-lanes.py",
]


# ── PR test-lane gating (ci.yml + ci-macos-vz.yml) ─────────────────────
# These gate the CI *test* lanes (not the bake lanes above) so a change only
# runs the lanes it can affect. Protos live in the engram-protocol crate, so a
# proto change is already in every Rust binary's closure (→ cc, host_binaries,
# e2e); the explicit `proto` flag is for the NON-cargo lanes (web/orchestrator
# codegen + buf). A change to a CI workflow file or this detector itself
# (`CI_SELF_PATHS`) forces ALL test lanes — the definition of "what runs"
# changed, so re-run everything.
CI_SELF_PATHS = [".github/workflows/ci.yml",
                 # Local composite actions are workflow steps by another
                 # name — editing one changes what the lanes run without
                 # touching ci.yml (ci-macos-vz.yml was folded into
                 # ci.yml; its old entry here was a dead path).
                 ".github/actions/",
                 ".github/scripts/detect-rebake-lanes.py"]
PROTO_PATHS = ["crates/engram-protocol/proto/", "buf.gen.yaml"]
WEB_PATHS = ["web/"]
ORCH_PATHS = ["orchestrator/"]
# A lockfile/manifest/toolchain bump recompiles the whole workspace.
RUST_COMMON = ["Cargo.lock", "Cargo.toml", "rust-toolchain.toml"]

# ── per-image bake selectivity ─────────────────────────────────────────
# The container-image matrix used to rebake ALL 5 images on any images-lane
# change. Compute per-image (release closure per binary + that image's own
# Dockerfile/sources) so a change scoped to one image rebakes ONLY it —
# helm-deploy resolves each image's deployable SHA independently from GHCR, so
# an unbaked image just keeps its previous SHA. A change to the bake workflow
# or this detector (BAKE_ALL_PATHS) re-bakes everything (the bake logic moved).
BAKE_ALL_PATHS = [".github/workflows/bake-images.yml",
                  ".github/scripts/detect-rebake-lanes.py"]


def cargo_meta():
    return json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1"]))


def release_closure(meta, bins):
    """Workspace crates reachable from `bins` via normal/build deps (not dev)."""
    members = set(meta["workspace_members"])
    id2name = {p["id"]: p["name"] for p in meta["packages"]}
    ws = {id2name[m] for m in members}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    stack = [p["id"] for p in meta["packages"] if p["name"] in bins and p["id"] in members]
    seen, out = set(), set()
    while stack:
        nid = stack.pop()
        if nid in seen:
            continue
        seen.add(nid)
        nm = id2name.get(nid)
        if nm in ws:
            out.add(nm)
        for d in nodes.get(nid, {}).get("deps", []):
            kinds = [k.get("kind") for k in d.get("dep_kinds", [])]
            # Skip dev-only edges: a dev-dependency is compiled for tests,
            # never linked into the release binary that bakes into the image.
            if kinds and all(k == "dev" for k in kinds):
                continue
            stack.append(d["pkg"])
    return out


def crate_dirs(meta, repo_root):
    """repo-relative crate dir -> crate name, for workspace members."""
    members = set(meta["workspace_members"])
    id2 = {p["id"]: p for p in meta["packages"]}
    out = {}
    for m in members:
        d = Path(id2[m]["manifest_path"]).parent.resolve()
        try:
            out[str(d.relative_to(repo_root))] = id2[m]["name"]
        except ValueError:
            pass
    return out


def changed_crates(changed, dirs):
    out = set()
    by_len = sorted(dirs, key=len, reverse=True)  # longest-prefix wins
    for f in changed:
        for d in by_len:
            if f == d or f.startswith(d + "/"):
                out.add(dirs[d])
                break
    return out


def any_path(changed, prefixes):
    return any(c == p or c.startswith(p) for c in changed for p in prefixes)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="")
    ap.add_argument("--head", default="HEAD")
    ap.add_argument("--stdin", action="store_true", help="read changed paths from stdin (testing)")
    args = ap.parse_args()

    if args.stdin:
        changed = [l.strip() for l in sys.stdin if l.strip()]
    else:
        # `before` is all-zeros on a branch's first push; fall back to the
        # head commit alone.
        if args.base and set(args.base) != {"0"}:
            rng = f"{args.base}..{args.head}"
        else:
            rng = f"{args.head}^!"
        changed = subprocess.check_output(["git", "diff", "--name-only", rng]).decode().split("\n")
        changed = [c for c in changed if c]

    meta = cargo_meta()
    repo_root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"]).decode().strip()).resolve()
    cc = changed_crates(changed, crate_dirs(meta, repo_root))
    fc = release_closure(meta, FC_BINS)
    cont = release_closure(meta, CONTAINER_BINS)
    harness = release_closure(meta, SESSION_HARNESS_BINS)
    agentd_closure = release_closure(meta, AGENTD_BINS)
    e2e_closure = release_closure(meta, E2E_BINS)
    host_sim_closure = release_closure(meta, HOST_SIM_BINS)
    cosim_closure = release_closure(meta, COSIM_BINS)

    # ADR 0045 Phase B: a Firecracker-fork bump (submodule pointer) restages the
    # FC binary in the node-assets image. It trips `images` so the notify job
    # fires engrams-changed → helm-deploy (the deploy trigger), and it trips the
    # narrower `node_assets` lane below so publish-node-assets actually rebakes.
    fc_fork = any_path(changed, FC_FORK_PATHS)
    images = bool(cc & cont) or any_path(changed, IMAGES_PATHS) or fc_fork
    host_binaries = bool(cc & fc) or any_path(changed, HOST_BINARIES_PATHS)
    host_base = any_path(changed, HOST_BASE_PATHS)
    # Union — gates the publish-host-binaries job so the GHCR artifact exists
    # at this SHA for whichever downstream bake (thin and/or base) fires.
    host_image = host_binaries or host_base
    # Golden cli artifact (cli-tools = the Bun-compiled `engrams` binary).
    # Republish whenever cli/ sources moved, the protos it codegens from
    # changed, or the bake workflow / this script changed. No cargo terms:
    # the orchestrator-native rewrite took the CLI out of the Rust workspace.
    cli_tools = (any_path(changed, CLI_PATHS)
                 or any_path(changed, PROTO_PATHS)
                 or any_path(changed, BAKE_ALL_PATHS))
    tf_or_helm = any_path(changed, TF_HELM_PATHS)
    harness_changed = bool(cc & harness) or any_path(changed, HARNESS_PATHS)
    # A harness-source change must republish the `bundle-harness-claude`
    # artifact: publish-bundles builds it from the engram-harness-claude tree
    # (ADR 0062), and node-assets stages it onto the fleet. Without this term a
    # harness change rebaked only the host-agent image and the fleet kept
    # staging the STALE harness bundle against a freshly-rolled host-agent — the
    # exact break that wedged harness attach after the #542 wire-10 roll (the
    # comment at SESSION_HARNESS_BINS promised this republish but it was never
    # wired).
    # ADR 0080: an agentd-source change republishes `bundle-agentd` — the
    # bundle IS the delivery vehicle (no image carries agentd), so without
    # this term an agentd change ships nothing.
    agentd_changed = bool(cc & agentd_closure)
    bundles = any_path(changed, BUNDLES_PATHS) or harness_changed or agentd_changed
    # The node-assets image bake gate. Its content is ONLY the pinned firecracker
    # binary (fc_fork), the pinned guest kernel + FC-version pins (the fetch
    # script), and the RO bundles (bundles) — NOTHING from the coordinator / web /
    # orchestrator / host-agent source trees. Gate publish-node-assets on THIS,
    # not on the coarse `images` lane, so a container-only change no longer
    # rebakes node-assets under a fresh SHA tag (which the deploy pins into the
    # host-fleet DaemonSet → a fleet-wide roll for no content change; the
    # 2026-07-20 incident). A bake-workflow / detector change (BAKE_ALL_PATHS)
    # re-bakes everything, node-assets included.
    node_assets = (
        fc_fork
        or bundles
        or any_path(changed, NODE_ASSETS_PATHS)
        or any_path(changed, BAKE_ALL_PATHS)
    )
    # The expensive e2e stack has two scopes. The core scope covers the Rust
    # stack and its setup inputs, including the two automation scenarios. The
    # orchestrator-only scope runs those scenarios in their own stack. This
    # keeps the normal full posture at two stacks while an orchestrator-only PR
    # does not also run the unrelated coordinator and evacuation scenarios.
    e2e_core = (
        bool(cc & e2e_closure)
        or any_path(changed, E2E_PATHS)
        or any_path(changed, HARNESS_PATHS)
    )
    e2e_orchestrator = e2e_core or any_path(changed, ORCH_PATHS)
    e2e = e2e_core or e2e_orchestrator

    # Keep #403 quarantined in the core suite. A 2026-07-17 un-quarantine run
    # still timed out after prompt delivery with the pinned Claude CLI, so the
    # failure is in the harness spawn/auth path rather than CLI shape drift.
    e2e_core_matrix = [
        {
            "variant": "suite",
            "two_hosts": "",
            "expect_two_hosts": "",
            "nextest_filter": (
                "test(/e2e_/) "
                "- test(e2e_two_host_evacuate_preserves_sentinel) "
                "- test(e2e_claude_with_bogus_key_surfaces_anthropic_auth_error)"
            ),
        },
        {
            "variant": "teleport",
            "two_hosts": "1",
            "expect_two_hosts": "1",
            "nextest_filter": "test(e2e_two_host_evacuate_preserves_sentinel)",
        },
    ]
    e2e_orchestrator_matrix = [{
        "variant": "orchestrator",
        "two_hosts": "",
        "expect_two_hosts": "",
        "nextest_filter": "test(/e2e_automation_/)",
    }]
    # Core changes use the normal two-stack posture. Only an orchestrator-only
    # change needs the dedicated orchestrator variant.
    if e2e_core:
        e2e_matrix = e2e_core_matrix
    elif e2e_orchestrator:
        e2e_matrix = e2e_orchestrator_matrix
    else:
        e2e_matrix = []
    # Pushes to main and merge-group runs retain the full e2e posture even if
    # their single-commit path set would select only one PR variant.
    e2e_full_matrix = e2e_core_matrix

    # ── PR test-lane gating ────────────────────────────────────────────
    ci_self = any_path(changed, CI_SELF_PATHS)
    proto = any_path(changed, PROTO_PATHS)
    # lint + tests(linux) + the two macOS lanes build/test the whole workspace:
    # any workspace crate (cc — protos included via engram-protocol), a
    # lockfile/toolchain bump, or a CI-definition change.
    test_rust = (
        ci_self
        or bool(cc)
        or any_path(changed, RUST_COMMON)
        or any_path(changed, HARNESS_PATHS)
    )
    # musl cross-compile of the FC host binaries — same closure as host_binaries.
    test_cross = ci_self or host_binaries
    # Firecracker integration lane. Gated on the e2e binary CLOSURE (the FC
    # backend + host-agent + their deps — the Rust the FC integration tests
    # exercise; engram-sandbox-firecracker is in host-agent's closure, and the
    # FC tests live in that crate) and the vendored FC fork — NOT the full `e2e`
    # flag. The e2e stack's non-crate inputs (deploy/bundles/, deploy/demo*,
    # Tiltfile, deploy/dev/) don't affect the FC tests, which use their own
    # fixtures and mount bundles via the sandbox crates already in the closure;
    # tripping FC on a bundle-payload edit was a needless ~KVM lane. The
    # e2e-STACK lane still gates on `e2e`, so bundle staging is validated there.
    test_fc = ci_self or bool(cc & e2e_closure) or fc_fork
    # web / orchestrator / cli: their own sources or the protos they codegen from.
    test_web = ci_self or proto or any_path(changed, WEB_PATHS)
    test_orchestrator = ci_self or proto or any_path(changed, ORCH_PATHS)
    test_cli = ci_self or proto or any_path(changed, CLI_PATHS)
    # buf only lints/breaking-checks/codegen-drifts the protos.
    test_buf = ci_self or proto
    # ADR 0098 P9: the host-sim swarm — its binary's own release closure.
    test_host_sim = ci_self or bool(cc & host_sim_closure)
    # ADR 0098 R-CoSim: the coordinator↔host boundary sim — its own (spanning)
    # release closure.
    test_cosim = ci_self or bool(cc & cosim_closure)

    # ── per-image bake selectivity ─────────────────────────────────────
    bake_all = any_path(changed, BAKE_ALL_PATHS)
    coord_closure = release_closure(meta, {"engram-coordinator"})
    ha_closure = release_closure(meta, {"engram-host-agent", "engram-uffd-handler"})
    hop_closure = release_closure(meta, {"engram-host-operator"})
    image_flags = {
        # Rust images: their release closure, own Dockerfile, or a lockfile bump
        # (migrations bake into the coord image specifically).
        "coordinator": bake_all or bool(cc & coord_closure)
        or any_path(changed, ["docker/coordinator.Dockerfile", "deploy/migrations/", "Cargo.lock", "Cargo.toml"]),
        "host-agent": bake_all or bool(cc & ha_closure)
        or any_path(changed, ["docker/host-agent.Dockerfile", "Cargo.lock", "Cargo.toml"]),
        "host-operator": bake_all or bool(cc & hop_closure)
        or any_path(changed, ["docker/host-operator.Dockerfile", "Cargo.lock", "Cargo.toml"]),
        # Bun images: own sources, own Dockerfile, or the protos they codegen.
        "web": bake_all or proto or any_path(changed, ["web/", "docker/web.Dockerfile"]),
        "orchestrator": bake_all or proto or any_path(changed, ["orchestrator/", "docker/orchestrator.Dockerfile"]),
    }
    # Stable matrix order; the bake job consumes this as `fromJSON`.
    images_matrix = [name for name in
                     ["coordinator", "host-agent", "host-operator", "web", "orchestrator"]
                     if image_flags[name]]

    print(f"changed files: {len(changed)}", file=sys.stderr)
    print(f"changed crates: {sorted(cc)}", file=sys.stderr)
    print(f"-> images={images} host_binaries={host_binaries} "
          f"host_base={host_base} host_image={host_image} cli_tools={cli_tools} "
          f"tf_or_helm={tf_or_helm} bundles={bundles} node_assets={node_assets} "
          f"fc_fork={fc_fork} e2e={e2e} e2e_core={e2e_core} "
          f"e2e_orchestrator={e2e_orchestrator}",
          file=sys.stderr)
    print(f"-> test_rust={test_rust} test_cross={test_cross} test_fc={test_fc} "
          f"test_web={test_web} test_orchestrator={test_orchestrator} "
          f"test_cli={test_cli} test_buf={test_buf} ci_self={ci_self} proto={proto} "
          f"test_host_sim={test_host_sim} test_cosim={test_cosim}",
          file=sys.stderr)
    print(f"-> images_matrix={images_matrix}", file=sys.stderr)
    print(f"-> e2e_matrix={e2e_matrix}", file=sys.stderr)

    def b(v):
        return 'true' if v else 'false'

    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a") as f:
            f.write(f"images={b(images)}\n")
            f.write(f"host_binaries={b(host_binaries)}\n")
            f.write(f"host_base={b(host_base)}\n")
            f.write(f"host_image={b(host_image)}\n")
            f.write(f"cli_tools={b(cli_tools)}\n")
            f.write(f"tf_or_helm={b(tf_or_helm)}\n")
            f.write(f"bundles={b(bundles)}\n")
            f.write(f"node_assets={b(node_assets)}\n")
            f.write(f"fc_fork={b(fc_fork)}\n")
            f.write(f"e2e={b(e2e)}\n")
            f.write(f"e2e_matrix={json.dumps({'include': e2e_matrix})}\n")
            f.write(f"e2e_full_matrix={json.dumps({'include': e2e_full_matrix})}\n")
            # PR test lanes (ci.yml + ci-macos-vz.yml gate on these).
            f.write(f"test_rust={b(test_rust)}\n")
            f.write(f"test_cross={b(test_cross)}\n")
            f.write(f"test_fc={b(test_fc)}\n")
            f.write(f"test_web={b(test_web)}\n")
            f.write(f"test_orchestrator={b(test_orchestrator)}\n")
            f.write(f"test_cli={b(test_cli)}\n")
            f.write(f"test_buf={b(test_buf)}\n")
            f.write(f"test_host_sim={b(test_host_sim)}\n")
            f.write(f"test_cosim={b(test_cosim)}\n")
            # Per-image bake matrix (JSON array → fromJSON in bake-images.yml).
            f.write(f"images_matrix={json.dumps(images_matrix)}\n")


if __name__ == "__main__":
    main()
