from __future__ import annotations

import importlib.util
import json
import pathlib
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


MODULE_PATH = pathlib.Path(__file__).parents[1] / "libexec" / "datadog.py"
SPEC = importlib.util.spec_from_file_location("engrams_datadog", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
datadog = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(datadog)


class Handler(BaseHTTPRequestHandler):
    calls: list[tuple[str, dict[str, str], dict]] = []

    def log_message(self, _format: str, *_args: object) -> None:
        pass

    def do_POST(self) -> None:
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.calls.append((self.path, dict(self.headers.items()), body))
        method = body["method"]
        if method == "notifications/initialized":
            self.send_response(202)
            self.end_headers()
            return
        if method == "initialize":
            result = {
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-datadog", "version": "1"},
            }
        elif method == "tools/list":
            result = {
                "tools": [
                    {
                        "name": "explore_profiling_flame_graph",
                        "inputSchema": {"type": "object"},
                    }
                ]
            }
        else:
            result = {"content": [{"type": "text", "text": "allocation result"}]}
        response = json.dumps({"jsonrpc": "2.0", "id": body["id"], "result": result}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        if method == "initialize":
            self.send_header("Mcp-Session-Id", "session-1")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)


class DatadogMcpClientTest(unittest.TestCase):
    def setUp(self) -> None:
        Handler.calls = []
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def client(self):
        host, port = self.server.server_address
        return datadog.McpClient(
            f"http://{host}:{port}/api/unstable/mcp-server/mcp?toolsets=profiling"
        )

    def test_lists_only_the_fixed_profiling_toolset_with_broker_placeholders(self) -> None:
        client = self.client()
        client.initialize()
        tools = client.list_tools()

        self.assertEqual(tools[0]["name"], "explore_profiling_flame_graph")
        self.assertEqual(
            {call[0] for call in Handler.calls},
            {"/api/unstable/mcp-server/mcp?toolsets=profiling"},
        )
        initialize_headers = {key.lower(): value for key, value in Handler.calls[0][1].items()}
        self.assertEqual(initialize_headers["dd_api_key"], "x-engrams-managed")
        self.assertEqual(initialize_headers["dd_application_key"], "x-engrams-managed")
        list_headers = {key.lower(): value for key, value in Handler.calls[-1][1].items()}
        self.assertEqual(list_headers["mcp-session-id"], "session-1")
        self.assertEqual(list_headers["mcp-method"], "tools/list")

    def test_tool_call_mirrors_the_tool_name_header(self) -> None:
        client = self.client()
        client.initialize()
        result = client.call_tool("explore_profiling_flame_graph", {"query": "service:brain-api"})

        self.assertEqual(result["content"][0]["text"], "allocation result")
        path, headers, body = Handler.calls[-1]
        self.assertEqual(path, "/api/unstable/mcp-server/mcp?toolsets=profiling")
        normalized = {key.lower(): value for key, value in headers.items()}
        self.assertEqual(normalized["mcp-name"], "explore_profiling_flame_graph")
        self.assertEqual(body["params"]["arguments"], {"query": "service:brain-api"})


if __name__ == "__main__":
    unittest.main()
