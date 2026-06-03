/// <reference types="vitest" />
import http from 'node:http';
import path from 'node:path';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Coordinator binds to 127.0.0.1:8090 in `just dev` / `just dev-vz`.
// The whole coord API lives under /api/v1, so proxy /api straight
// through to the coord; everything else (incl. the SPA's /sessions/:id
// deep links) is served by Vite as index.html.
const COORDINATOR = process.env.ENGRAM_COORDINATOR_URL ?? 'http://127.0.0.1:8090';

// http-proxy defaults to `agent: false`, which forces a fresh TCP
// connection plus `Connection: close` on every proxied request. On
// macOS each closed loopback socket holds an ephemeral port in
// TIME_WAIT for 2*MSL=30s against a ~16k-port pool — combined with
// Tilt's readiness probes that's enough to wedge connects in
// SYN_SENT during normal SPA use. Pool through a keep-alive agent so
// vite reuses one socket per route.
const proxyAgent = new http.Agent({ keepAlive: true });

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { '@': path.resolve(__dirname, './src') } },
  server: {
    port: 5173,
    proxy: {
      // The entire coord API (REST + SSE + the /shell WebSocket) lives
      // under /api/v1, which no longer overlaps any SPA route — so a
      // single prefix proxy suffices and the old Accept-header bypass is
      // gone. `ws: true` carries the /api/v1/sessions/:id/shell upgrade;
      // http-proxy streams SSE through unchanged.
      '/api': {
        target: COORDINATOR,
        changeOrigin: true,
        agent: proxyAgent,
        ws: true,
      },
    },
  },
  test: {
    environment: 'jsdom',
    globals: false,
    setupFiles: ['./src/test-setup.ts'],
    include: ['src/**/*.{test,spec}.{ts,tsx}'],
  },
});
