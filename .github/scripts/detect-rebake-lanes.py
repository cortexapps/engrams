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
  cli_tools     engram-cli + engram-agentd changed — release closure of
                {engram-cli, engram-agentd}. Gates the OSS publish-cli-tools
                job, which republishes the "golden" cli+agentd GHCR artifact
                (cli-tools) that the reusable bake-dev-image workflow pulls
                instead of recompiling. Same role publish-host-binaries plays
                for the FC-host bakes.
  tf_or_helm    deploy/terraform/ + deploy/helm/
  dev_image     dev-engrams dogfood rebake — images OR host_binaries OR
                host_base OR the release closure of {engram-harness-claude}
                (baked into the image, not pulled at runtime) OR the
                dev-orchestration inputs. Fires `engrams-dev-image-changed`.

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
# anything in its release closure) must republish that artifact + re-bake the
# dev_image lane (whose `just dev` stages the harness from source). It is in
# NEITHER the container nor the host closure, so without this lane a
# harness-only change shipped nothing (the bug this gate fixes).
SESSION_HARNESS_BINS = {"engram-harness-claude"}
# The ops CLI + the in-guest agent injected into session images at bake time.
# OSS publishes them once as the `cli-tools` GHCR artifact (publish-cli-tools);
# the reusable bake-dev-image workflow pulls that instead of compiling from a
# source checkout — exactly how the FC-host bakes consume publish-host-binaries.
# agentd must match the deployed coordinator, so a change to either binary (or
# anything in its release closure) must republish cli-tools.
CLI_TOOLS_BINS = {"engram-cli", "engram-agentd"}
# The binaries the `test-e2e-stack` lane builds + boots: coord + host-agent
# (the stack), cli (drives enable/registry), agentd (injected into the demo
# image), and harness-claude (staged into the host bundle stamp; ADR 0062). A
# change anywhere in their release closure means the e2e lane could behave
# differently, so run it. Gates the (expensive, non-required) e2e lane.
E2E_BINS = {
    "engram-coordinator",
    "engram-host-agent",
    "engram-cli",
    "engram-agentd",
    "engram-harness-claude",
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
BUNDLES_PATHS = ["deploy/bundles/"]
# ADR 0027: the `dev-engrams` dogfood session image runs the REAL `just dev`
# (whole-repo build) inside a sandbox, so it's stale on essentially any source
# change. We trip its rebake on the union of what it builds — the container +
# host source closures (computed below) plus the dev-orchestration inputs
# here. Doc/TF-only pushes don't rebake it.
DEV_IMAGE_PATHS = ["justfile", "flake.nix", "flake.lock", "Tiltfile", "deploy/dev/"]
# ADR 0045 Phase B: the vendored Firecracker fork (a submodule + the `.gitmodules`
# gitlink). Bumping the submodule pointer (the daily auto-rebase, or a manual
# port) changes the FC *binary* the node-assets image stages, so it must rebuild
# node-assets + roll the host fleet — folded into the `images` lane below.
# Inert until the submodule exists (these paths don't change today).
FC_FORK_PATHS = ["third_party/firecracker", ".gitmodules"]
# Non-crate inputs to the `test-e2e-stack` lane: the dev-orchestration
# scripts + Tiltfile that bring the stack up, the demo image sources it
# bakes, the RO bundles it stages, and the workflow / detector themselves.
# (The e2e test file lives under crates/engram-coordinator/, so it's already
# covered by that crate being in E2E_BINS' closure.)
E2E_PATHS = [
    "deploy/dev/",
    "Tiltfile",
    "deploy/demo/",
    "deploy/bundles/",
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
                 ".github/workflows/ci-macos-vz.yml",
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
    cli_tools_closure = release_closure(meta, CLI_TOOLS_BINS)
    e2e_closure = release_closure(meta, E2E_BINS)

    # ADR 0045 Phase B: a Firecracker-fork bump (submodule pointer) restages the
    # FC binary in the node-assets image, so it trips the images lane (which
    # gates publish-node-assets → the operator's drain-gated host roll).
    fc_fork = any_path(changed, FC_FORK_PATHS)
    images = bool(cc & cont) or any_path(changed, IMAGES_PATHS) or fc_fork
    host_binaries = bool(cc & fc) or any_path(changed, HOST_BINARIES_PATHS)
    host_base = any_path(changed, HOST_BASE_PATHS)
    # Union — gates the publish-host-binaries job so the GHCR artifact exists
    # at this SHA for whichever downstream bake (thin and/or base) fires.
    host_image = host_binaries or host_base
    # Golden cli+agentd artifact (cli-tools). Republish whenever either binary's
    # release closure moved, or a conservative common trigger (lockfile / root
    # manifest / the bake workflow / this script) changed, OR the flake changed:
    # publish-cli-tools ALSO bundles a pinned static mke2fs built via
    # `nix build .#mke2fs-static` (ADR 0036), so flake.nix/flake.lock feed the
    # artifact even when no Rust binary moved. Without this, an e2fsprogs re-pin
    # leaves cli-tools carrying the stale mke2fs and the dogfood bakes don't see
    # the fix (exactly what happened with the 1.47.3 -> 1.47.2 pin).
    cli_tools = (bool(cc & cli_tools_closure)
                 or any_path(changed, BINARY_COMMON)
                 or any_path(changed, ["flake.nix", "flake.lock"]))
    tf_or_helm = any_path(changed, TF_HELM_PATHS)
    bundles = any_path(changed, BUNDLES_PATHS)
    # The dogfood image builds the whole repo via `just dev`, so it's stale on
    # any source the container/host bakes consume, plus the dev-orchestration
    # inputs. It ALSO bakes in the builtin claude harness, so a harness-only
    # change (in neither container nor host closure) must rebake it too —
    # without this term, a harness change shipped nothing. NOT tripped by
    # doc/TF/helm-only pushes.
    harness_changed = bool(cc & harness)
    dev_image = (
        images
        or host_binaries
        or host_base
        or harness_changed
        or any_path(changed, DEV_IMAGE_PATHS)
    )
    # The expensive, non-required e2e lane: run it when any binary it builds
    # moved or any of its non-crate inputs changed. Skipping it on
    # doc/TF/web-only PRs is the win; a false positive just runs it
    # needlessly (safe), so this errs toward running.
    e2e = bool(cc & e2e_closure) or any_path(changed, E2E_PATHS)

    # ── PR test-lane gating ────────────────────────────────────────────
    ci_self = any_path(changed, CI_SELF_PATHS)
    proto = any_path(changed, PROTO_PATHS)
    # lint + tests(linux) + the two macOS lanes build/test the whole workspace:
    # any workspace crate (cc — protos included via engram-protocol), a
    # lockfile/toolchain bump, or a CI-definition change.
    test_rust = ci_self or bool(cc) or any_path(changed, RUST_COMMON)
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
    # web / orchestrator: their own sources or the protos they codegen from.
    test_web = ci_self or proto or any_path(changed, WEB_PATHS)
    test_orchestrator = ci_self or proto or any_path(changed, ORCH_PATHS)
    # buf only lints/breaking-checks/codegen-drifts the protos.
    test_buf = ci_self or proto

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
          f"tf_or_helm={tf_or_helm} bundles={bundles} dev_image={dev_image} "
          f"fc_fork={fc_fork} e2e={e2e}",
          file=sys.stderr)
    print(f"-> test_rust={test_rust} test_cross={test_cross} test_fc={test_fc} "
          f"test_web={test_web} test_orchestrator={test_orchestrator} "
          f"test_buf={test_buf} ci_self={ci_self} proto={proto}",
          file=sys.stderr)
    print(f"-> images_matrix={images_matrix}", file=sys.stderr)

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
            f.write(f"dev_image={b(dev_image)}\n")
            f.write(f"fc_fork={b(fc_fork)}\n")
            f.write(f"e2e={b(e2e)}\n")
            # PR test lanes (ci.yml + ci-macos-vz.yml gate on these).
            f.write(f"test_rust={b(test_rust)}\n")
            f.write(f"test_cross={b(test_cross)}\n")
            f.write(f"test_fc={b(test_fc)}\n")
            f.write(f"test_web={b(test_web)}\n")
            f.write(f"test_orchestrator={b(test_orchestrator)}\n")
            f.write(f"test_buf={b(test_buf)}\n")
            # Per-image bake matrix (JSON array → fromJSON in bake-images.yml).
            f.write(f"images_matrix={json.dumps(images_matrix)}\n")


if __name__ == "__main__":
    main()
