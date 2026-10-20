#!/usr/bin/env bash
# File (or update) the GitHub issue for a failed nightly sim swarm.
#
# Usage: sim-failure-issue.sh <title prefix> <default slug> <fix flow> <report>...
#
# Turns the sim's --failure-report artifacts (violation, pinned replay
# command, trace tail per failed seed) into one issue. Dedup: one open
# issue per invariant slug under the given title prefix; a repeat
# failure of the same invariant becomes a comment on it, so a recurring
# bug accumulates seeds instead of spamming issues. The prefix keeps the
# coordinator, host and cosim swarms' same-named invariants apart.
#
# A job that failed WITHOUT a report (a build break, a package mirror
# outage, a runner death, a timeout) is an infra error, not a finding:
# the job's own red status is the signal, and no issue is filed.
set -euo pipefail

prefix=$1
default_slug=$2
fix_flow=$3
shift 3

reports=""
for f in "$@"; do
  if [ -s "$f" ]; then
    reports="${reports}$(cat "$f")"$'\n'
  fi
done
if [ -z "$reports" ]; then
  echo "no failure report: the job failed before the swarm could report, so this is an infra error and no issue is filed"
  exit 0
fi

slug=$(printf '%s' "$reports" | sed -n 's/^<!-- sim-failure-slug: \(.*\) -->$/\1/p' | head -1)
slug=${slug:-$default_slug}
title="${prefix}: \`${slug}\` violated"
run_url="${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}"
body=$(printf '%s\n\n%s\n\n---\n_Run: %s • commit %s • fix flow: %s_\n' \
  "**Commit:** \`${GITHUB_SHA}\` — replay commands below assume \`git checkout ${GITHUB_SHA}\`." \
  "$reports" \
  "$run_url" \
  "${GITHUB_SHA}" \
  "$fix_flow")
existing=$(gh issue list --state open --label sim-failure --search "\"$title\" in:title" --json number --jq '.[0].number // empty')
if [ -n "$existing" ]; then
  echo "commenting on existing issue #$existing"
  gh issue comment "$existing" --body "$body"
else
  gh issue create --title "$title" --label sim-failure --body "$body"
fi
