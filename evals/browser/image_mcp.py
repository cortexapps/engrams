#!/usr/bin/env python3
"""Tiny stdio MCP server that makes eval screenshots model-visible.

This mirrors the production harness-native browser_view tool. It reads only
from roots passed on argv, returns image content to the model, and performs no
sharing or network I/O.
"""

from __future__ import annotations

import base64
import json
from pathlib import Path
import sys
import os


MAX_IMAGE_BYTES = 12 * 1024 * 1024


def error(request_id: object, code: int, message: str) -> dict[str, object]:
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def mime_type(data: bytes) -> str:
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return "image/png"
    if data.startswith(b"\xff\xd8\xff"):
        return "image/jpeg"
    if len(data) >= 12 and data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    raise ValueError("browser_view supports PNG, JPEG, and WebP content only")


def load_image(path_text: str, roots: list[Path]) -> tuple[str, str]:
    path = Path(path_text)
    if not path.is_absolute():
        raise ValueError("browser_view path must be absolute")
    path = path.resolve(strict=True)
    allowed = any(path.is_relative_to(root.resolve(strict=True)) for root in roots)
    if not allowed:
        raise ValueError("browser_view path is outside the episode observation roots")
    if not path.is_file():
        raise ValueError("browser_view path is not a regular file")
    if path.stat().st_size > MAX_IMAGE_BYTES:
        raise ValueError(f"browser_view image exceeds {MAX_IMAGE_BYTES} bytes")
    data = path.read_bytes()
    return mime_type(data), base64.b64encode(data).decode("ascii")


def handle(request: dict[str, object], roots: list[Path]) -> dict[str, object] | None:
    method = request.get("method")
    if method and str(method).startswith("notifications/"):
        return None
    request_id = request.get("id")
    if method == "initialize":
        params = request.get("params")
        version = params.get("protocolVersion", "2025-06-18") if isinstance(params, dict) else "2025-06-18"
        result = {
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "browser-eval-view", "version": "1"},
        }
    elif method == "tools/list":
        result = {
            "tools": [
                {
                    "name": "browser_view",
                    "description": "Inspect a browser screenshot as an internal visual observation. This does not share the image with the user.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"],
                        "additionalProperties": False,
                    },
                }
            ]
        }
    elif method == "tools/call":
        params = request.get("params")
        if not isinstance(params, dict) or params.get("name") != "browser_view":
            return error(request_id, -32602, "Unknown tool")
        arguments = params.get("arguments")
        path = arguments.get("path") if isinstance(arguments, dict) else None
        if not isinstance(path, str):
            result = {"content": [{"type": "text", "text": "browser_view requires a string path"}], "isError": True}
        else:
            try:
                mime, data = load_image(path, roots)
                pending = os.environ.get("ENGRAM_EVAL_PENDING_VIEW")
                if pending:
                    Path(pending).unlink(missing_ok=True)
                # Codex applies an MCP-specific environment policy, so do not
                # rely on inherited ENGRAM_EVAL_* variables. The episode root
                # is also an allowed image root and owns the interlock file.
                for root in roots:
                    (root / "pending-view").unlink(missing_ok=True)
                result = {
                    "content": [
                        {"type": "text", "text": "Internal browser observation; not shared with the user."},
                        {"type": "image", "data": data, "mimeType": mime},
                    ]
                }
            except (OSError, ValueError) as exc:
                result = {"content": [{"type": "text", "text": str(exc)}], "isError": True}
    else:
        return error(request_id, -32601, "Method not found")
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def main() -> int:
    roots = [Path(arg) for arg in sys.argv[1:]]
    if not roots:
        print("image_mcp.py requires at least one allowed root", file=sys.stderr)
        return 2
    for line in sys.stdin:
        try:
            request = json.loads(line)
            response = handle(request, roots)
        except json.JSONDecodeError:
            response = error(None, -32700, "Parse error")
        if response is not None:
            print(json.dumps(response, separators=(",", ":")), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
