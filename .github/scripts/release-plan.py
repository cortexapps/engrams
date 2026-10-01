#!/usr/bin/env python3
"""Decide which published build of each artifact a release promotes.

A release builds nothing. bake-images.yml publishes every artifact under the
full SHA of the main commit it was built from, and it rebuilds an artifact only
when a commit changes that artifact's inputs. A release gives each of those
builds a version tag, so the bytes that a release ships are the bytes that CI
tested on main.

For each artifact this script:

  1. Reads the artifact's SHA tags from GHCR.
  2. Selects the build from the nearest commit at or before the release commit
     (the newest SHA tag that is an ancestor of it).
  3. Asks detect-rebake-lanes.py if the artifact's inputs changed between that
     commit and the release commit. A change means that a bake is in progress,
     failed, or was cancelled. The release stops: a version tag on a stale
     build is worse than no release.

The output is one `<artifact>\t<sha>` line per artifact, which release.yml
reads to do the promotion.

Usage (CI):   release-plan.py --repo cortexapps/engrams --sha "$SHA" --out plan.tsv
Usage (test): release-plan.py --repo o/r --sha HEAD --tags-json tags.json --out plan.tsv
              where tags.json is {"<artifact>": ["<sha>", ...], ...}
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

DETECTOR = Path(__file__).with_name("detect-rebake-lanes.py")

# The five container images whose lanes the detector evaluates against a
# registry baseline (`--image-baseline`), in the detector's own order.
BASELINE_IMAGES = ["coordinator", "host-agent", "host-operator", "web", "orchestrator"]
# Artifacts with a push-range lane: the detector output that says "the inputs
# changed in this range".
RANGE_LANES = {"node-assets": "node_assets", "cli-tools": "cli_tools"}
ARTIFACTS = BASELINE_IMAGES + list(RANGE_LANES)

SHA_RE = re.compile(r"^[0-9a-f]{40}$")


def git(*args):
    return subprocess.check_output(["git", *args]).decode().strip()


def published_shas(repo, artifact):
    """Every full-SHA tag of one GHCR package."""
    owner, name = repo.split("/", 1)
    out = subprocess.check_output([
        "gh", "api", "--paginate",
        f"/orgs/{owner}/packages/container/{name}%2F{artifact}/versions?per_page=100",
        "--jq", ".[] | .metadata.container.tags[]?",
    ]).decode()
    return {t for t in out.split() if SHA_RE.match(t)}


def nearest_build(history, shas):
    """The first commit in `history` (newest first) that has a published build."""
    for commit in history:
        if commit in shas:
            return commit
    return None


def detector_outputs(*args):
    """Run the lane detector and return its GITHUB_OUTPUT as a dict."""
    with tempfile.NamedTemporaryFile(mode="r", suffix=".out") as f:
        env = dict(os.environ, GITHUB_OUTPUT=f.name)
        subprocess.run([sys.executable, str(DETECTOR), *args], check=True, env=env)
        return dict(line.rstrip("\n").split("=", 1) for line in f if "=" in line)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True, help="owner/name of the repository")
    ap.add_argument("--sha", required=True, help="the commit to release")
    ap.add_argument("--out", required=True, help="where to write the plan")
    ap.add_argument("--tags-json", help="read SHA tags from a file, not GHCR (testing)")
    args = ap.parse_args()

    release = git("rev-parse", f"{args.sha}^{{commit}}")
    history = git("rev-list", "--first-parent", release).split("\n")

    if args.tags_json:
        tags = {k: set(v) for k, v in json.loads(Path(args.tags_json).read_text()).items()}
    else:
        tags = {a: published_shas(args.repo, a) for a in ARTIFACTS}

    plan, errors = {}, []
    for artifact in ARTIFACTS:
        build = nearest_build(history, tags.get(artifact, set()))
        if build is None:
            errors.append(f"{artifact}: no published build at or before {release[:12]}")
            continue
        plan[artifact] = build
        print(f"{artifact}: build {build[:12]}", file=sys.stderr)

    # Container images: an empty push range plus one baseline per image makes
    # the detector report exactly the images whose inputs changed since their
    # selected build.
    baselines = [a for a in BASELINE_IMAGES if a in plan]
    if baselines:
        out = detector_outputs(
            "--base", release, "--head", release,
            *[arg for a in baselines for arg in ("--image-baseline", f"{a}={plan[a]}")])
        for artifact in json.loads(out["images_matrix"]):
            errors.append(f"{artifact}: inputs changed after build {plan[artifact][:12]}")

    for artifact, lane in RANGE_LANES.items():
        if artifact not in plan:
            continue
        out = detector_outputs("--base", plan[artifact], "--head", release)
        if out[lane] == "true":
            errors.append(f"{artifact}: inputs changed after build {plan[artifact][:12]}")

    if errors:
        for e in errors:
            print(f"::error::{e}", file=sys.stderr)
        print(
            "The registry is behind the release commit. Wait for the 'Bake container "
            "images' run of that commit to finish. If the run failed or was cancelled, "
            "start that workflow by hand on main, then start the release again.",
            file=sys.stderr)
        return 1

    Path(args.out).write_text("".join(f"{a}\t{plan[a]}\n" for a in ARTIFACTS))
    return 0


if __name__ == "__main__":
    sys.exit(main())
