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
                + web/ + docker/ + migrations/
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

# Binaries that bake into each artifact. uffd-handler ships ONLY on the FC
# host image (spawned by host-agent), never in a container — so it gates the
# host lanes but not images.
FC_BINS = {"engram-host-agent", "engram-uffd-handler"}
# engram-host-operator (ADR 0044 K3) bakes into its own container image; add
# it so an operator-only crate change rebuilds the images lane.
CONTAINER_BINS = {"engram-coordinator", "engram-host-agent", "engram-host-operator"}
# Binaries baked INTO session images at image-build time by
# engram-image-builder::inject_builtin_harness (pulled from the
# harness-claude GHCR pack, written to /sbin/engram-harness-claude). The
# dev-engrams + demo-claude images bake this in — they do NOT pull it at
# runtime — so a change to the harness binary (or anything in its release
# closure) must re-bake those images. It is in NEITHER container nor host
# closure, so without this lane a harness-only change shipped nothing: the
# dev-engrams rebake never fired (the bug this fixes). It gates only the
# dev_image lane below — the demo image bakes every push regardless.
SESSION_HARNESS_BINS = {"engram-harness-claude"}
# The ops CLI + the in-guest agent injected into session images at bake time.
# OSS publishes them once as the `cli-tools` GHCR artifact (publish-cli-tools);
# the reusable bake-dev-image workflow pulls that instead of compiling from a
# source checkout — exactly how the FC-host bakes consume publish-host-binaries.
# agentd must match the deployed coordinator, so a change to either binary (or
# anything in its release closure) must republish cli-tools.
CLI_TOOLS_BINS = {"engram-cli", "engram-agentd"}

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
IMAGES_PATHS = ["docker/", "web/", "deploy/migrations/"] + BINARY_COMMON
TF_HELM_PATHS = ["deploy/terraform/", "deploy/helm/"]
# ADR 0027: the RO session bundles (skills / playwright). A change here means
# the bundle artifacts must be rebuilt + republished, and the FC-host image
# re-baked to pull the new squashfs. Independent of the Rust/OS lanes.
BUNDLES_PATHS = ["deploy/bundles/"]
# ADR 0027: the `dev-engrams` dogfood session image runs the REAL `just dev`
# (whole-repo build) inside a sandbox, so it's stale on essentially any source
# change. We trip its rebake on the union of what it builds — the container +
# host source closures (computed below) plus the dev-orchestration inputs
# here. Doc/TF-only pushes don't rebake it.
DEV_IMAGE_PATHS = ["justfile", "flake.nix", "flake.lock", "Tiltfile", "deploy/dev/"]


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

    images = bool(cc & cont) or any_path(changed, IMAGES_PATHS)
    host_binaries = bool(cc & fc) or any_path(changed, HOST_BINARIES_PATHS)
    host_base = any_path(changed, HOST_BASE_PATHS)
    # Union — gates the publish-host-binaries job so the GHCR artifact exists
    # at this SHA for whichever downstream bake (thin and/or base) fires.
    host_image = host_binaries or host_base
    # Golden cli+agentd artifact (cli-tools). Republish whenever either binary's
    # release closure moved, or a conservative common trigger (lockfile / root
    # manifest / the bake workflow / this script) changed.
    cli_tools = bool(cc & cli_tools_closure) or any_path(changed, BINARY_COMMON)
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

    print(f"changed files: {len(changed)}", file=sys.stderr)
    print(f"changed crates: {sorted(cc)}", file=sys.stderr)
    print(f"-> images={images} host_binaries={host_binaries} "
          f"host_base={host_base} host_image={host_image} cli_tools={cli_tools} "
          f"tf_or_helm={tf_or_helm} bundles={bundles} dev_image={dev_image}",
          file=sys.stderr)

    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a") as f:
            f.write(f"images={'true' if images else 'false'}\n")
            f.write(f"host_binaries={'true' if host_binaries else 'false'}\n")
            f.write(f"host_base={'true' if host_base else 'false'}\n")
            f.write(f"host_image={'true' if host_image else 'false'}\n")
            f.write(f"cli_tools={'true' if cli_tools else 'false'}\n")
            f.write(f"tf_or_helm={'true' if tf_or_helm else 'false'}\n")
            f.write(f"bundles={'true' if bundles else 'false'}\n")
            f.write(f"dev_image={'true' if dev_image else 'false'}\n")


if __name__ == "__main__":
    main()
