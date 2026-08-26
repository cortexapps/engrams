#!/usr/bin/env python3
"""Source fingerprint of engram-egress-proxyd's dependency closure (ADR 0121).

The node-local egress daemon outlives host-agent pods; the successor
pod replaces it ONLY when the daemon's code actually changed. This
script computes the "actually changed" signal: a content hash over the
daemon's release dependency closure, injected into BOTH binaries at
image build via the ENGRAM_EGRESS_PROXYD_FINGERPRINT env (`option_env!`).

Never a binary hash: builds are not reproducible, so a binary hash
would restart the daemon on ~every deploy and defeat the design.

Hash inputs, deterministic by construction:
  1. (name, version) of every NON-workspace package in the closure —
     an external bump (tokio, rustls) is a real behavior change.
  2. File contents of every workspace-member crate dir in the closure,
     sorted by path. Hidden files are skipped (editor droppings must
     not perturb the fingerprint).

workspace-hack is deliberately EXCLUDED from input 2: its generated
Cargo.toml churns on any workspace-wide dep change, and feature
unification drift alone is not a reason to cut a node's streams. Its
external packages still land in input 1 through the closure.

Runs anywhere `cargo metadata` runs (the Docker builder stage installs
python3 for it; the musl bake lane has both on the runner).
"""

import hashlib
import json
import subprocess
import sys
from pathlib import Path

ROOT_PKG = "engram-egress-proxyd"
EXCLUDED_WORKSPACE_DIRS = {"workspace-hack"}


def main() -> int:
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked"],
        )
    )
    packages = {p["id"]: p for p in meta["packages"]}
    workspace = set(meta["workspace_members"])
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    roots = [p["id"] for p in meta["packages"] if p["name"] == ROOT_PKG]
    if len(roots) != 1:
        print(f"expected exactly one {ROOT_PKG} package, found {len(roots)}", file=sys.stderr)
        return 1

    # Release closure: normal + build deps, never dev (tests do not
    # ship in the binary). dep_kinds' kind is None for normal deps.
    closure: set[str] = set()
    stack = [roots[0]]
    while stack:
        pid = stack.pop()
        if pid in closure:
            continue
        closure.add(pid)
        for dep in nodes[pid].get("deps", []):
            kinds = dep.get("dep_kinds") or [{}]
            if any(k.get("kind") in (None, "build") for k in kinds):
                stack.append(dep["pkg"])

    h = hashlib.sha256()
    for pid in sorted(closure - workspace):
        p = packages[pid]
        h.update(f"{p['name']} {p['version']}\n".encode())
    repo_root = Path(meta["workspace_root"])
    for pid in sorted(closure & workspace):
        crate_dir = Path(packages[pid]["manifest_path"]).parent
        if crate_dir.name in EXCLUDED_WORKSPACE_DIRS:
            continue
        for f in sorted(crate_dir.rglob("*")):
            if not f.is_file() or any(part.startswith(".") for part in f.parts):
                continue
            if "target" in f.relative_to(crate_dir).parts:
                continue
            h.update(str(f.relative_to(repo_root)).encode() + b"\0")
            h.update(f.read_bytes())
            h.update(b"\0")

    print(h.hexdigest())
    return 0


if __name__ == "__main__":
    sys.exit(main())
