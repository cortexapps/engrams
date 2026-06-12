/// <reference types="vitest" />
import http from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// ghostty-web embeds the WASM as a data: URI in its JS bundle, but the
// SPA's CSP (connect-src 'self') blocks fetch() of data: URIs, causing
// the library's fallback candidates — ./ghostty-vt.wasm and
// /ghostty-vt.wasm — to be tried instead. Without this plugin both
// paths return the SPA's index.html (nginx try_files catch-all), which
// passes C.ok === true but contains HTML, making WebAssembly.compile()
// throw "expected magic word 00 61 73 6d, found 3c 21 64 6f".
//
// Fix: serve the real binary at /ghostty-vt.wasm in dev AND copy it to
// the dist root for production so nginx serves the file before reaching
// the try_files fallback.
function ghosttyWasmPlugin(): Plugin {
  const wasmSrc = "node_modules/ghostty-web/ghostty-vt.wasm";
  return {
    name: "ghostty-wasm",
    configureServer(server) {
      server.middlewares.use("/ghostty-vt.wasm", async (_req, res) => {
        try {
          const buf = await readFile(wasmSrc);
          res.setHeader("Content-Type", "application/wasm");
          res.end(buf);
        } catch {
          res.statusCode = 404;
          res.end("not found");
        }
      });
    },
    async generateBundle() {
      this.emitFile({
        type: "asset",
        fileName: "ghostty-vt.wasm",
        source: await readFile(wasmSrc),
      });
    },
  };
}

// Coordinator binds to 127.0.0.1:8090 in `just dev` / `just dev-vz`.
// The whole coord API lives under /api/v1, so proxy /api straight
// through to the coord; everything else (incl. the SPA's /sessions/:id
// deep links) is served by Vite as index.html.
const COORDINATOR = process.env.ENGRAM_COORDINATOR_URL ?? "http://127.0.0.1:8090";

// Orchestrator binds to 127.0.0.1:8787. The following paths route to it:
//   /api/auth   — better-auth session endpoints (sign-in, sign-up, sign-out)
//   /rpc        — connect-query tRPC/gRPC bridge (Task 23+)
//   /api/v1/me/claude-token — sealed-store token presence (exact path; see below)
// Task 28 collapses the /api/v1/me/claude-token special rule when all of
// /api/v1 moves to the orchestrator and the coordinator rule is removed.
const ORCHESTRATOR = process.env.ENGRAM_ORCHESTRATOR_URL ?? "http://127.0.0.1:8787";

// http-proxy defaults to `agent: false`, which forces a fresh TCP
// connection plus `Connection: close` on every proxied request. On
// macOS each closed loopback socket holds an ephemeral port in
// TIME_WAIT for 2*MSL=30s against a ~16k-port pool — combined with
// Tilt's readiness probes that's enough to wedge connects in
// SYN_SENT during normal SPA use. Pool through a keep-alive agent so
// vite reuses one socket per route.
const proxyAgent = new http.Agent({ keepAlive: true });

export default defineConfig({
  plugins: [react(), tailwindcss(), ghosttyWasmPlugin()],
  resolve: { alias: { "@": path.resolve(__dirname, "./src") } },
  server: {
    port: 5173,
    proxy: {
      // ---- Orchestrator routes (must come BEFORE the coordinator /api rule) ----
      //
      // Vite matches proxy rules in insertion order. These exact / prefix rules
      // must be listed BEFORE '/api' so they shadow the coordinator catch-all.

      // better-auth session endpoints: sign-in, sign-up, sign-out, get-session.
      "/api/auth": {
        target: ORCHESTRATOR,
        changeOrigin: true,
        agent: proxyAgent,
      },

      // connect-query / tRPC bridge (Task 23+).
      "/rpc": {
        target: ORCHESTRATOR,
        changeOrigin: true,
        agent: proxyAgent,
        ws: true,
      },

      // Exact path: token presence + CRUD from the orchestrator sealed-store.
      // NOTE: Vite does NOT match exact paths natively — prefix match on
      // '/api/v1/me/claude-token' means any path that STARTS WITH this string.
      // That's fine: there are no sub-paths under /api/v1/me/claude-token/…
      // Task 28 removes this rule when /api/v1 fully moves to the orchestrator.
      "/api/v1/me/claude-token": {
        target: ORCHESTRATOR,
        changeOrigin: true,
        agent: proxyAgent,
      },

      // Task 26: orchestrator SSE event feed for session detail.
      // Vite proxy keys support regex strings (^/regex/). This rule must
      // come BEFORE the coordinator /api catch-all so the orchestrator
      // envelope is served instead of the coordinator's direct-wire shape.
      // Task 28 removes this rule when all of /api/v1 moves to 8787.
      "^/api/v1/sessions/[^/]+/events": {
        target: ORCHESTRATOR,
        changeOrigin: true,
        agent: proxyAgent,
      },

      // Task 27: orchestrator shell WebSocket relay.
      // The TerminalPane connects to /api/v1/sessions/:id/shell with the
      // 'tty' subprotocol; the orchestrator bridges it to the host's ttyd.
      // Regex must come BEFORE the coordinator /api catch-all.
      // Task 28 removes this rule when all of /api/v1 moves to the orchestrator.
      "^/api/v1/sessions/[^/]+/shell": {
        target: ORCHESTRATOR,
        changeOrigin: true,
        ws: true,
      },

      // ---- Coordinator catch-all (REST + SSE) ----
      //
      // All remaining /api/v1 traffic proxies to the coordinator until Task 28
      // migrates the full surface to the orchestrator. ws:true retained for
      // any other coordinator WS paths (none remain post-Task 27).
      "/api": {
        target: COORDINATOR,
        changeOrigin: true,
        agent: proxyAgent,
        ws: true,
      },
    },
  },
  test: {
    environment: "jsdom",
    globals: false,
    setupFiles: ["./src/test-setup.ts"],
    include: ["src/**/*.{test,spec}.{ts,tsx}"],
  },
});
