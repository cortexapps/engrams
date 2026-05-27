#!/usr/bin/env python3
"""Decide which deploy lanes a change set affects, from the cargo dep graph.

Replaces the hand-maintained path denylist in bake-images.yml's `detect`
job (which silently drifts: a crate that host-agent/uffd-handler actually
depends on can get wrongly denied, shipping a stale FC binary).

A lane is affected iff a changed file maps to a crate in that lane's
binary **release** dependency closure (dev-deps excluded — they don't
ship), or a changed file matches a non-crate path rule for the lane.

Lanes:
  images      container rebake — release closure of {coordinator, host-agent}
              + web/ + docker/ + migrations/
  host_image  FC host GCE image — release closure of {host-agent, uffd-handler}
              + deploy/packer/ + rust-toolchain.toml
  tf_or_helm  deploy/terraform/ + deploy/helm/

`Cargo.lock` / root `Cargo.toml` / this script / the bake workflow are
conservative triggers for both binary lanes.

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
# host image (spawned by host-agent), never in a container — so it gates
# host_image but not images.
FC_BINS = {"engram-host-agent", "engram-uffd-handler"}
CONTAINER_BINS = {"engram-coordinator", "engram-host-agent"}

# Non-crate path prefixes per lane. `Cargo.lock`/root `Cargo.toml`/this
# script/the bake workflow conservatively trip both binary lanes.
BINARY_COMMON = ["Cargo.lock", "Cargo.toml",
                 ".github/workflows/bake-images.yml",
                 ".github/scripts/detect-rebake-lanes.py"]
HOST_IMAGE_PATHS = ["deploy/packer/", "rust-toolchain.toml"] + BINARY_COMMON
IMAGES_PATHS = ["docker/", "web/", "deploy/migrations/"] + BINARY_COMMON
TF_HELM_PATHS = ["deploy/terraform/", "deploy/helm/"]


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

    images = bool(cc & cont) or any_path(changed, IMAGES_PATHS)
    host_image = bool(cc & fc) or any_path(changed, HOST_IMAGE_PATHS)
    tf_or_helm = any_path(changed, TF_HELM_PATHS)

    print(f"changed files: {len(changed)}", file=sys.stderr)
    print(f"changed crates: {sorted(cc)}", file=sys.stderr)
    print(f"-> images={images} host_image={host_image} tf_or_helm={tf_or_helm}", file=sys.stderr)

    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a") as f:
            f.write(f"images={'true' if images else 'false'}\n")
            f.write(f"host_image={'true' if host_image else 'false'}\n")
            f.write(f"tf_or_helm={'true' if tf_or_helm else 'false'}\n")


if __name__ == "__main__":
    main()
