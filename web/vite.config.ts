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

// ADR 0051 Task 28: all browser traffic goes to the orchestrator (:8787).
// The coordinator (:8090) is no longer a browser target. Its REST routes
// remain live for engram-cli / integration scripts (Task 29) but the browser
// never hits them directly after this proxy flip.
//
// Both /api and /rpc collapse into two orchestrator-only rules:
//   /rpc  — Connect/gRPC bridge (connect-query). ws:true retained for the
//            shell WebSocket relay (path: /api/v1/sessions/:id/shell).
//   /api  — Hono routes: auth, events SSE, artifacts, /me/claude-token,
//            admin REST proxy (pause/resume). ws:true carries the shell WS
//            upgrade through this catch-all (the shell path matches /api/v1/…).
//
// The per-path orchestrator rules added in Tasks 22–27 (exact /api/auth,
// /api/v1/me/claude-token, regex events, regex shell) are all subsumed by the
// catch-all and removed.
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
      // ---- Orchestrator: Connect/gRPC bridge ----
      "/rpc": {
        target: ORCHESTRATOR,
        changeOrigin: true,
        agent: proxyAgent,
        ws: true,
      },

      // ---- Orchestrator: all /api traffic (auth, events, artifacts, me,
      //      admin, shell WS) ----
      // ws:true is required so the shell WebSocket upgrade
      // (/api/v1/sessions/:id/shell) is forwarded correctly.
      "/api": {
        target: ORCHESTRATOR,
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
