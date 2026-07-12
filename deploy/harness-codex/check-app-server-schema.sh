#!/usr/bin/env bash
set -euo pipefail

codex="${1:?usage: check-app-server-schema.sh <codex-bin>}"
out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT

"$codex" app-server generate-json-schema --out "$out"
for schema in \
  v2/ThreadStartParams.json \
  v2/ThreadResumeParams.json \
  v2/TurnStartParams.json \
  v2/TurnSteerParams.json \
  v2/TurnInterruptParams.json \
  v2/TurnCompletedNotification.json \
  v2/AgentMessageDeltaNotification.json \
  v2/FileChangePatchUpdatedNotification.json \
  v2/ThreadNameUpdatedNotification.json \
  ToolRequestUserInputParams.json; do
  test -s "$out/$schema" || { echo "Codex stable app-server schema missing $schema" >&2; exit 1; }
done

jq -e '.properties.clientUserMessageId and .properties.expectedTurnId' "$out/v2/TurnSteerParams.json" >/dev/null
jq -e '.properties.turnId and .properties.threadId' "$out/v2/TurnInterruptParams.json" >/dev/null
jq -e '.properties.delta and .properties.itemId' "$out/v2/AgentMessageDeltaNotification.json" >/dev/null
jq -e '.properties.threadId and .properties.threadName' "$out/v2/ThreadNameUpdatedNotification.json" >/dev/null
jq -e '.definitions.TurnStatus.enum | contains(["completed", "failed", "interrupted", "inProgress"])' \
  "$out/ServerNotification.json" >/dev/null
