#!/usr/bin/env python3
"""Engrams-owned Datadog REST and Continuous Profiler CLI."""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from typing import Any


API_URL = os.environ.get("DATADOG_API_URL", "https://api.datadoghq.com")
MCP_URL = os.environ.get(
    "DATADOG_MCP_URL",
    "https://mcp.datadoghq.com/api/unstable/mcp-server/mcp?toolsets=profiling",
)
API_KEY = os.environ.get("DD_API_KEY", "x-engrams-managed")
APP_KEY = os.environ.get("DD_APP_KEY", "x-engrams-managed")
CLIENT_NAME = "engrams-datadog"
CLIENT_VERSION = "1.0.0"
REQUEST_TIMEOUT_SECONDS = 60


class DatadogError(RuntimeError):
    """A Datadog HTTP or MCP protocol error safe to show to the agent."""


def print_json(value: Any) -> None:
    print(json.dumps(value, indent=2, sort_keys=True))


def decode_response(body: bytes, content_type: str, request_id: int | None) -> Any:
    if not body:
        return None
    text = body.decode("utf-8", "replace")
    if "text/event-stream" not in content_type.lower():
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            return text

    # Streamable HTTP can return one or more JSON-RPC messages as SSE events.
    # Select the response for this request ID and ignore keepalives/comments.
    events: list[Any] = []
    data: list[str] = []
    for line in text.splitlines() + [""]:
        if line == "":
            if data:
                events.append(json.loads("\n".join(data)))
                data = []
            continue
        if line.startswith("data:"):
            data.append(line[5:].lstrip())
    if request_id is None:
        return events[0] if events else None
    for event in events:
        if isinstance(event, dict) and event.get("id") == request_id:
            return event
    raise DatadogError(f"Datadog MCP returned no response for request {request_id}")


def open_request(request: urllib.request.Request) -> tuple[Any, Any]:
    try:
        with urllib.request.urlopen(request, timeout=REQUEST_TIMEOUT_SECONDS) as response:
            return response.headers, decode_response(
                response.read(), response.headers.get("Content-Type", ""), None
            )
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace").strip()
        suffix = f": {detail}" if detail else ""
        raise DatadogError(f"Datadog returned HTTP {error.code}{suffix}") from error
    except urllib.error.URLError as error:
        raise DatadogError(f"Datadog request failed: {error.reason}") from error


def api_request(method: str, path: str, body: str | None) -> Any:
    if not path.startswith("/") or path.startswith("//"):
        raise DatadogError("API path must start with one '/' and must not be a URL")
    data = body.encode() if body is not None else None
    headers = {
        "Accept": "application/json",
        "DD-API-KEY": API_KEY,
        "DD-APPLICATION-KEY": APP_KEY,
        "User-Agent": f"{CLIENT_NAME}/{CLIENT_VERSION}",
    }
    if data is not None:
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(
        f"{API_URL.rstrip('/')}{path}", data=data, headers=headers, method=method.upper()
    )
    response_headers, decoded = open_request(request)
    _ = response_headers
    return decoded


class McpClient:
    """Small Streamable HTTP client for Datadog's profiling-only endpoint."""

    def __init__(self, endpoint: str = MCP_URL) -> None:
        self.endpoint = endpoint
        self.session_id: str | None = None
        self.protocol_version = "2025-11-25"
        self.next_id = 1

    def _request(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        *,
        notification: bool = False,
        tool_name: str | None = None,
    ) -> Any:
        request_id = None if notification else self.next_id
        if request_id is not None:
            self.next_id += 1
        message: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if request_id is not None:
            message["id"] = request_id
        if params is not None:
            message["params"] = params

        headers = {
            "Accept": "application/json, text/event-stream",
            "Content-Type": "application/json",
            "DD_API_KEY": API_KEY,
            "DD_APPLICATION_KEY": APP_KEY,
            "Mcp-Method": method,
            "MCP-Protocol-Version": self.protocol_version,
            "User-Agent": f"{CLIENT_NAME}/{CLIENT_VERSION}",
        }
        if self.session_id:
            headers["Mcp-Session-Id"] = self.session_id
        if tool_name:
            headers["Mcp-Name"] = tool_name

        request = urllib.request.Request(
            self.endpoint,
            data=json.dumps(message, separators=(",", ":")).encode(),
            headers=headers,
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=REQUEST_TIMEOUT_SECONDS) as response:
                if response.headers.get("Mcp-Session-Id"):
                    self.session_id = response.headers["Mcp-Session-Id"]
                decoded = decode_response(
                    response.read(), response.headers.get("Content-Type", ""), request_id
                )
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", "replace").strip()
            suffix = f": {detail}" if detail else ""
            raise DatadogError(f"Datadog MCP returned HTTP {error.code}{suffix}") from error
        except urllib.error.URLError as error:
            raise DatadogError(f"Datadog MCP request failed: {error.reason}") from error

        if isinstance(decoded, dict) and "error" in decoded:
            error = decoded["error"]
            raise DatadogError(f"Datadog MCP error: {json.dumps(error, sort_keys=True)}")
        return decoded.get("result") if isinstance(decoded, dict) else decoded

    def initialize(self) -> None:
        result = self._request(
            "initialize",
            {
                "protocolVersion": self.protocol_version,
                "capabilities": {},
                "clientInfo": {"name": CLIENT_NAME, "version": CLIENT_VERSION},
            },
        )
        if not isinstance(result, dict):
            raise DatadogError("Datadog MCP returned an invalid initialize result")
        negotiated = result.get("protocolVersion")
        if isinstance(negotiated, str) and negotiated:
            self.protocol_version = negotiated
        self._request("notifications/initialized", notification=True)

    def list_tools(self) -> list[dict[str, Any]]:
        tools: list[dict[str, Any]] = []
        cursor: str | None = None
        while True:
            params = {"cursor": cursor} if cursor else None
            result = self._request("tools/list", params)
            if not isinstance(result, dict) or not isinstance(result.get("tools"), list):
                raise DatadogError("Datadog MCP returned an invalid tools/list result")
            tools.extend(tool for tool in result["tools"] if isinstance(tool, dict))
            cursor = result.get("nextCursor")
            if not isinstance(cursor, str) or not cursor:
                return tools

    def call_tool(self, name: str, arguments: dict[str, Any]) -> Any:
        result = self._request(
            "tools/call", {"name": name, "arguments": arguments}, tool_name=name
        )
        if not isinstance(result, dict):
            raise DatadogError("Datadog MCP returned an invalid tools/call result")
        return result


def parse_arguments_json(raw: str) -> dict[str, Any]:
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise DatadogError(f"arguments must be valid JSON: {error}") from error
    if not isinstance(value, dict):
        raise DatadogError("arguments JSON must be an object")
    return value


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Query Datadog with platform-brokered authentication."
    )
    commands = parser.add_subparsers(dest="command", required=True)

    api = commands.add_parser("api", help="Call a capability-gated Datadog REST API path.")
    api.add_argument("method", help="HTTP method, for example GET or POST.")
    api.add_argument("path", help="Absolute API path, including any query string.")
    api.add_argument("body", nargs="?", help="Optional JSON request body.")

    profiling = commands.add_parser(
        "profiling", help="Use the Datadog Continuous Profiler MCP toolset."
    )
    profiling_commands = profiling.add_subparsers(dest="profiling_command", required=True)
    profiling_commands.add_parser("tools", help="List profiling tools and input schemas.")
    schema = profiling_commands.add_parser("schema", help="Show one profiling tool schema.")
    schema.add_argument("tool")
    call = profiling_commands.add_parser("call", help="Call one profiling tool.")
    call.add_argument("tool")
    call.add_argument("arguments", nargs="?", default="{}", help="Tool arguments as JSON.")
    return parser


def main() -> int:
    args = build_parser().parse_args()
    try:
        if args.command == "api":
            result = api_request(args.method, args.path, args.body)
            if result is not None:
                print_json(result)
            return 0

        client = McpClient()
        client.initialize()
        if args.profiling_command == "tools":
            print_json(client.list_tools())
            return 0
        if args.profiling_command == "schema":
            tool = next((tool for tool in client.list_tools() if tool.get("name") == args.tool), None)
            if tool is None:
                raise DatadogError(f"profiling tool not found: {args.tool}")
            print_json(tool)
            return 0

        result = client.call_tool(args.tool, parse_arguments_json(args.arguments))
        print_json(result)
        return 1 if result.get("isError") is True else 0
    except DatadogError as error:
        print(f"datadog: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
