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
  ToolRequestUserInputParams.json \
  v1/InitializeParams.json \
  DynamicToolCallParams.json \
  DynamicToolCallResponse.json; do
  test -s "$out/$schema" || { echo "Codex stable app-server schema missing $schema" >&2; exit 1; }
done

jq -e '.properties.clientUserMessageId and .properties.expectedTurnId' "$out/v2/TurnSteerParams.json" >/dev/null
jq -e '.properties.turnId and .properties.threadId' "$out/v2/TurnInterruptParams.json" >/dev/null
jq -e '.properties.delta and .properties.itemId' "$out/v2/AgentMessageDeltaNotification.json" >/dev/null
jq -e '.properties.threadId and .properties.threadName' "$out/v2/ThreadNameUpdatedNotification.json" >/dev/null
jq -e '.definitions.TurnStatus.enum | contains(["completed", "failed", "interrupted", "inProgress"])' \
  "$out/ServerNotification.json" >/dev/null

# ADR 0089 P3, pinned from live codex 0.144.1 generated schemas.
jq -e '.definitions.InitializeCapabilities.properties.experimentalApi.type == "boolean"' \
  "$out/v1/InitializeParams.json" >/dev/null
jq -e '
  (.required | contains(["arguments", "callId", "threadId", "tool", "turnId"])) and
  (.properties.arguments == true) and
  (.properties.callId.type == "string") and
  (.properties.threadId.type == "string") and
  (.properties.tool.type == "string") and
  (.properties.turnId.type == "string")
' "$out/DynamicToolCallParams.json" >/dev/null
jq -e '
  (.required | contains(["contentItems", "success"])) and
  (.properties.success.type == "boolean") and
  (.properties.contentItems.type == "array") and
  (.properties.contentItems.items["$ref"] == "#/definitions/DynamicToolCallOutputContentItem") and
  any(
    .definitions.DynamicToolCallOutputContentItem.oneOf[];
    (.required | contains(["text", "type"])) and
    (.properties.text.type == "string") and
    (.properties.type.enum | contains(["inputText"]))
  )
' "$out/DynamicToolCallResponse.json" >/dev/null
